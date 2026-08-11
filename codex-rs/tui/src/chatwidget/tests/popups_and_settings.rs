use super::*;
use codex_features::Stage;
use pretty_assertions::assert_eq;

#[tokio::test]
async fn experimental_features_popup_snapshot() {
    let (mut chat, _rx, _op_rx) = make_chatwidget_manual(/*model_override*/ None).await;

    let features = vec![
        ExperimentalFeatureItem {
            feature: Feature::JsRepl,
            name: "JavaScript REPL".to_string(),
            description: "Enable a persistent Node-backed JavaScript REPL for interactive website debugging and other inline JavaScript execution capabilities.".to_string(),
            enabled: false,
        },
        ExperimentalFeatureItem {
            feature: Feature::ShellTool,
            name: "Shell tool".to_string(),
            description: "Allow the model to run shell commands.".to_string(),
            enabled: true,
        },
    ];
    let view = ExperimentalFeaturesView::new(
        features,
        chat.app_event_tx.clone(),
        crate::keymap::RuntimeKeymap::defaults().list,
    );
    chat.bottom_pane.show_view(Box::new(view));

    let popup = render_bottom_popup(&chat, /*width*/ 80);
    assert_chatwidget_snapshot!("experimental_features_popup", popup);

    let mut config = codex_config::types::TuiKeymap::default();
    config.list.accept = Some(codex_config::types::KeybindingsSpec::One(
        codex_config::types::KeybindingSpec("ctrl-x enter".to_string()),
    ));
    let keymap = crate::keymap::RuntimeKeymap::from_config(&config)
        .expect("valid experimental-feature chord");
    let view = ExperimentalFeaturesView::new(
        vec![ExperimentalFeatureItem {
            feature: Feature::ShellTool,
            name: "Shell tool".to_string(),
            description: "Allow the model to run shell commands.".to_string(),
            enabled: true,
        }],
        chat.app_event_tx.clone(),
        keymap.list,
    );
    chat.bottom_pane.show_view(Box::new(view));
    let popup = render_bottom_popup(&chat, /*width*/ 80);
    assert_chatwidget_snapshot!("experimental_features_popup_configured_key_chords", popup);
}

#[tokio::test]
async fn experimental_features_toggle_saves_on_exit() {
    let (mut chat, mut rx, _op_rx) = make_chatwidget_manual(/*model_override*/ None).await;

    let expected_feature = Feature::JsRepl;
    let view = ExperimentalFeaturesView::new(
        vec![ExperimentalFeatureItem {
            feature: expected_feature,
            name: "JavaScript REPL".to_string(),
            description: "Enable a persistent Node-backed JavaScript REPL for interactive website debugging and other inline JavaScript execution capabilities.".to_string(),
            enabled: false,
        }],
        chat.app_event_tx.clone(),
        crate::keymap::RuntimeKeymap::defaults().list,
    );
    chat.bottom_pane.show_view(Box::new(view));

    chat.handle_key_event(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE));

    assert!(
        rx.try_recv().is_err(),
        "expected no updates until saving the popup"
    );

    chat.handle_key_event(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

    let mut updates = None;
    while let Ok(event) = rx.try_recv() {
        if let AppEvent::UpdateFeatureFlags {
            updates: event_updates,
        } = event
        {
            updates = Some(event_updates);
            break;
        }
    }

    let updates = updates.expect("expected UpdateFeatureFlags event");
    assert_eq!(updates, vec![(expected_feature, true)]);
}

#[tokio::test]
async fn experimental_popup_omits_stable_guardian_approval() {
    let (mut chat, _rx, _op_rx) = make_chatwidget_manual(/*model_override*/ None).await;
    let guardian_stage = FEATURES
        .iter()
        .find(|spec| spec.id == Feature::GuardianApproval)
        .map(|spec| spec.stage)
        .expect("expected guardian approval feature metadata");

    assert_eq!(guardian_stage, Stage::Stable);

    chat.open_experimental_popup();

    let popup = render_bottom_popup(&chat, /*width*/ 120);
    assert!(
        !popup.contains("Auto-review"),
        "expected stable auto-review feature to be omitted from experimental popup, got:\n{popup}"
    );
}

#[tokio::test]
async fn multi_agent_enable_prompt_snapshot() {
    let (mut chat, _rx, _op_rx) = make_chatwidget_manual(/*model_override*/ None).await;

    chat.open_multi_agent_enable_prompt();

    let popup = render_bottom_popup(&chat, /*width*/ 80);
    assert_chatwidget_snapshot!("multi_agent_enable_prompt", popup);
}

#[tokio::test]
async fn multi_agent_enable_prompt_updates_feature_and_emits_notice() {
    let (mut chat, mut rx, _op_rx) = make_chatwidget_manual(/*model_override*/ None).await;

    chat.open_multi_agent_enable_prompt();
    chat.handle_key_event(KeyEvent::from(KeyCode::Enter));

    assert_matches!(
        rx.try_recv(),
        Ok(AppEvent::UpdateFeatureFlags { updates }) if updates == vec![(Feature::Collab, true)]
    );
    let cell = match rx.try_recv() {
        Ok(AppEvent::InsertHistoryCell(cell)) => cell,
        other => panic!("expected InsertHistoryCell event, got {other:?}"),
    };
    let rendered = lines_to_single_string(&cell.display_lines(/*width*/ 120));
    assert!(rendered.contains("Subagents will be enabled in the next session."));
}

#[tokio::test]
async fn memories_enable_prompt_snapshot() {
    let (mut chat, _rx, _op_rx) = make_chatwidget_manual(/*model_override*/ None).await;
    chat.set_feature_enabled(Feature::MemoryTool, /*enabled*/ false);

    chat.open_memories_popup();

    let popup = render_bottom_popup(&chat, /*width*/ 80);
    assert_chatwidget_snapshot!("memories_enable_prompt", popup);
}

#[tokio::test]
async fn memories_enable_prompt_updates_feature_without_notice() {
    let (mut chat, mut rx, _op_rx) = make_chatwidget_manual(/*model_override*/ None).await;
    chat.set_feature_enabled(Feature::MemoryTool, /*enabled*/ false);

    chat.open_memories_popup();
    chat.handle_key_event(KeyEvent::from(KeyCode::Enter));

    assert_matches!(
        rx.try_recv(),
        Ok(AppEvent::UpdateFeatureFlags { updates }) if updates == vec![(Feature::MemoryTool, true)]
    );
    assert!(
        rx.try_recv().is_err(),
        "memory enable prompt should not emit the success notice before persistence succeeds"
    );
}

#[tokio::test]
async fn memories_settings_popup_snapshot() {
    let (mut chat, _rx, _op_rx) = make_chatwidget_manual(/*model_override*/ None).await;
    chat.set_feature_enabled(Feature::MemoryTool, /*enabled*/ true);
    chat.config.memories.use_memories = true;
    chat.config.memories.generate_memories = false;

    chat.open_memories_popup();

    let popup = strip_osc8_for_snapshot(&render_bottom_popup(&chat, /*width*/ 80));
    assert_chatwidget_snapshot!("memories_settings_popup", popup);
}

#[tokio::test]
async fn memories_reset_confirmation_snapshot() {
    let (mut chat, _rx, _op_rx) = make_chatwidget_manual(/*model_override*/ None).await;
    chat.set_feature_enabled(Feature::MemoryTool, /*enabled*/ true);
    chat.config.memories.use_memories = true;
    chat.config.memories.generate_memories = false;

    chat.open_memories_popup();
    chat.handle_key_event(KeyEvent::from(KeyCode::Down));
    chat.handle_key_event(KeyEvent::from(KeyCode::Down));
    chat.handle_key_event(KeyEvent::from(KeyCode::Enter));

    let popup = render_bottom_popup(&chat, /*width*/ 80);
    assert_chatwidget_snapshot!("memories_reset_confirmation", popup);
}

#[tokio::test]
async fn memories_settings_toggle_saves_on_enter() {
    let (mut chat, mut rx, _op_rx) = make_chatwidget_manual(/*model_override*/ None).await;
    chat.set_feature_enabled(Feature::MemoryTool, /*enabled*/ true);
    chat.config.memories.use_memories = true;
    chat.config.memories.generate_memories = false;

    chat.open_memories_popup();
    chat.handle_key_event(KeyEvent::from(KeyCode::Down));
    chat.handle_key_event(KeyEvent::from(KeyCode::Char(' ')));
    chat.handle_key_event(KeyEvent::from(KeyCode::Enter));

    assert_matches!(
        rx.try_recv(),
        Ok(AppEvent::UpdateMemorySettings {
            use_memories: true,
            generate_memories: true,
        })
    );
}

#[tokio::test]
async fn memories_reset_confirmation_sends_event_on_confirm() {
    let (mut chat, mut rx, _op_rx) = make_chatwidget_manual(/*model_override*/ None).await;
    chat.set_feature_enabled(Feature::MemoryTool, /*enabled*/ true);
    chat.config.memories.use_memories = true;
    chat.config.memories.generate_memories = false;

    chat.open_memories_popup();
    chat.handle_key_event(KeyEvent::from(KeyCode::Down));
    chat.handle_key_event(KeyEvent::from(KeyCode::Down));
    chat.handle_key_event(KeyEvent::from(KeyCode::Enter));
    chat.handle_key_event(KeyEvent::from(KeyCode::Enter));

    assert_matches!(rx.try_recv(), Ok(AppEvent::ResetMemories));
}

#[tokio::test]
async fn model_selection_popup_snapshot() {
    let (mut chat, _rx, _op_rx) = make_chatwidget_manual(Some("gpt-5.2")).await;
    chat.thread_id = Some(ThreadId::new());
    chat.open_model_popup();

    let popup = render_bottom_popup(&chat, /*width*/ 80);
    assert_chatwidget_snapshot!("model_selection_popup", popup);
}

#[tokio::test]
async fn personality_selection_popup_snapshot() {
    let (mut chat, _rx, _op_rx) = make_chatwidget_manual(Some("gpt-5.4")).await;
    chat.thread_id = Some(ThreadId::new());
    chat.open_personality_popup();

    let popup = render_bottom_popup(&chat, /*width*/ 80);
    assert_chatwidget_snapshot!("personality_selection_popup", popup);
}

#[tokio::test]
async fn skills_menu_default_mentions_shortcut_snapshot() {
    let (mut chat, _rx, _op_rx) = make_chatwidget_manual(/*model_override*/ None).await;
    chat.open_skills_menu();

    let popup = render_bottom_popup(&chat, /*width*/ 80);
    assert_chatwidget_snapshot!("skills_menu_default_mentions_shortcut", popup);
}

#[tokio::test]
async fn model_picker_hides_show_in_picker_false_models_from_cache() {
    let (mut chat, _rx, _op_rx) = make_chatwidget_manual(Some("test-visible-model")).await;
    chat.thread_id = Some(ThreadId::new());
    let preset = |slug: &str, show_in_picker: bool| ModelPreset {
        id: slug.to_string(),
        model: slug.to_string(),
        display_name: slug.to_string(),
        description: format!("{slug} description"),
        model_specialty: None,
        default_reasoning_effort: ReasoningEffortConfig::Medium,
        supported_reasoning_efforts: vec![ReasoningEffortPreset {
            effort: ReasoningEffortConfig::Medium,
            description: "medium".to_string(),
        }],
        supports_personality: false,
        additional_speed_tiers: Vec::new(),
        service_tiers: Vec::new(),
        default_service_tier: None,
        is_default: false,
        upgrade: None,
        show_in_picker,
        multi_agent_version: None,
        availability_nux: None,
        supported_in_api: true,
        input_modalities: default_input_modalities(),
    };

    chat.open_model_popup_with_presets(vec![
        preset("test-visible-model", true),
        preset("test-hidden-model", false),
    ]);
    let popup = render_bottom_popup(&chat, /*width*/ 80);
    assert_chatwidget_snapshot!("model_picker_filters_hidden_models", popup);
    assert!(
        popup.contains("test-visible-model"),
        "expected visible model to appear in picker:\n{popup}"
    );
    assert!(
        !popup.contains("test-hidden-model"),
        "expected hidden model to be excluded from picker:\n{popup}"
    );
}

#[tokio::test]
async fn server_overloaded_error_does_not_switch_models() {
    let (mut chat, mut rx, mut op_rx) = make_chatwidget_manual(Some("gpt-5.2")).await;
    chat.set_model("gpt-5.2");
    while rx.try_recv().is_ok() {}
    while op_rx.try_recv().is_ok() {}

    handle_error(
        &mut chat,
        "server overloaded",
        Some(CodexErrorInfo::ServerOverloaded),
    );

    while let Ok(event) = rx.try_recv() {
        if let AppEvent::UpdateModel(model) = event {
            assert_eq!(
                model, "gpt-5.2",
                "did not expect model switch on server-overloaded error"
            );
        }
    }

    while let Ok(event) = op_rx.try_recv() {
        if let Op::OverrideTurnContext { model, .. } = event {
            assert!(
                model.is_none(),
                "did not expect OverrideTurnContext model update on server-overloaded error"
            );
        }
    }
}

#[tokio::test]
async fn model_reasoning_selection_popup_snapshot() {
    let (mut chat, _rx, _op_rx) = make_chatwidget_manual(Some("gpt-5.4")).await;

    set_chatgpt_auth(&mut chat);
    chat.set_reasoning_effort(Some(ReasoningEffortConfig::High));

    let mut preset = get_available_model(&chat, "gpt-5.4");
    preset.supported_reasoning_efforts.insert(
        2,
        ReasoningEffortPreset {
            effort: ReasoningEffortConfig::Max,
            description: "Maximum available reasoning".to_string(),
        },
    );
    preset
        .supported_reasoning_efforts
        .push(ReasoningEffortPreset {
            effort: ReasoningEffortConfig::Ultra,
            description: "Ultra reasoning".to_string(),
        });
    chat.open_reasoning_popup(preset);

    let popup = render_bottom_popup(&chat, /*width*/ 80);
    assert_chatwidget_snapshot!("model_reasoning_selection_popup", popup);
}

#[tokio::test]
async fn model_advanced_reasoning_selection_popup_snapshot() {
    let (mut chat, _rx, _op_rx) = make_chatwidget_manual(Some("gpt-5.4")).await;
    chat.set_reasoning_effort(Some(ReasoningEffortConfig::Ultra));

    let mut preset = get_available_model(&chat, "gpt-5.4");
    preset.supported_reasoning_efforts.extend([
        ReasoningEffortPreset {
            effort: ReasoningEffortConfig::Ultra,
            description: "Ultra reasoning".to_string(),
        },
        ReasoningEffortPreset {
            effort: ReasoningEffortConfig::Max,
            description: "Maximum available reasoning".to_string(),
        },
    ]);
    chat.open_advanced_reasoning_popup(preset);

    let popup = render_bottom_popup(&chat, /*width*/ 80);
    assert_chatwidget_snapshot!("model_advanced_reasoning_selection_popup", popup);
}

#[tokio::test]
async fn model_reasoning_selection_popup_applies_custom_effort() {
    let (mut chat, mut rx, _op_rx) = make_chatwidget_manual(Some("gpt-5.4")).await;
    let custom_effort = ReasoningEffortConfig::Custom("future".to_string());
    chat.set_reasoning_effort(Some(ReasoningEffortConfig::XHigh));

    let mut preset = get_available_model(&chat, "gpt-5.4");
    preset
        .supported_reasoning_efforts
        .push(ReasoningEffortPreset {
            effort: custom_effort.clone(),
            description: "Maximum available reasoning".to_string(),
        });
    chat.open_reasoning_popup(preset);
    while rx.try_recv().is_ok() {}

    chat.handle_key_event(KeyEvent::from(KeyCode::Down));
    chat.handle_key_event(KeyEvent::from(KeyCode::Enter));

    let selected_effort_events = std::iter::from_fn(|| rx.try_recv().ok())
        .filter_map(|event| match event {
            AppEvent::UpdateReasoningEffort(effort) => Some((None, effort)),
            AppEvent::PersistModelSelection { model, effort } => Some((Some(model), effort)),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        selected_effort_events,
        vec![
            (None, Some(custom_effort.clone())),
            (Some("gpt-5.4".to_string()), Some(custom_effort)),
        ]
    );
}

async fn select_ultra_with_multi_agent_thread_limit(max_threads: usize) -> (bool, Vec<String>) {
    let (mut chat, mut rx, _op_rx) = make_chatwidget_manual(Some("gpt-5.4")).await;
    chat.config
        .multi_agent_v2
        .max_concurrent_threads_per_session = max_threads;
    chat.set_reasoning_effort(Some(ReasoningEffortConfig::High));

    let mut preset = get_available_model(&chat, "gpt-5.4");
    preset.default_reasoning_effort = ReasoningEffortConfig::High;
    preset.supported_reasoning_efforts = vec![
        ReasoningEffortPreset {
            effort: ReasoningEffortConfig::High,
            description: "High reasoning".to_string(),
        },
        ReasoningEffortPreset {
            effort: ReasoningEffortConfig::Ultra,
            description: "Ultra reasoning".to_string(),
        },
    ];
    chat.open_reasoning_popup(preset);
    while rx.try_recv().is_ok() {}

    chat.handle_key_event(KeyEvent::from(KeyCode::Down));
    chat.handle_key_event(KeyEvent::from(KeyCode::Enter));

    let advanced_preset = std::iter::from_fn(|| rx.try_recv().ok()).find_map(|event| match event {
        AppEvent::OpenAdvancedReasoningPopup { model } => Some(model),
        _ => None,
    });
    chat.open_advanced_reasoning_popup(advanced_preset.expect("advanced reasoning popup"));
    chat.handle_key_event(KeyEvent::from(KeyCode::Enter));

    let mut selected_ultra = false;
    let mut warnings = Vec::new();
    while let Ok(event) = rx.try_recv() {
        match event {
            AppEvent::ApplyAdvancedReasoning {
                effort: ReasoningEffortConfig::Ultra,
                ..
            } => {
                selected_ultra = true;
            }
            AppEvent::InsertHistoryCell(cell) => {
                warnings.push(lines_to_single_string(&cell.display_lines(/*width*/ 80)));
            }
            _ => {}
        }
    }

    (selected_ultra, warnings)
}

#[tokio::test]
async fn ultra_reasoning_selection_warns_for_high_multi_agent_concurrency() {
    let (selected_ultra, warnings) =
        select_ultra_with_multi_agent_thread_limit(/*max_threads*/ 8).await;

    assert!(selected_ultra);
    assert_eq!(warnings.len(), 1);
    assert_chatwidget_snapshot!(
        "ultra_reasoning_selection_high_multi_agent_concurrency_warning",
        &warnings[0]
    );
}

#[tokio::test]
async fn ultra_reasoning_selection_skips_warning_below_threshold() {
    let below_threshold = select_ultra_with_multi_agent_thread_limit(/*max_threads*/ 7).await;

    assert_eq!(below_threshold, (true, Vec::new()));
}

#[tokio::test]
async fn max_reasoning_selection_persists_model_selection() {
    let (mut chat, mut rx, _op_rx) = make_chatwidget_manual(Some("gpt-5.4")).await;
    chat.set_reasoning_effort(Some(ReasoningEffortConfig::High));

    let mut preset = get_available_model(&chat, "gpt-5.4");
    preset.supported_reasoning_efforts = vec![ReasoningEffortPreset {
        effort: ReasoningEffortConfig::Max,
        description: "Maximum reasoning".to_string(),
    }];
    chat.open_advanced_reasoning_popup(preset);
    chat.handle_key_event(KeyEvent::from(KeyCode::Enter));

    let events = std::iter::from_fn(|| rx.try_recv().ok()).collect::<Vec<_>>();
    assert!(events.iter().any(|event| matches!(
        event,
        AppEvent::UpdateReasoningEffort(Some(ReasoningEffortConfig::Max))
    )));
    assert!(events.iter().any(|event| matches!(
        event,
        AppEvent::PersistModelSelection {
            model,
            effort: Some(ReasoningEffortConfig::Max),
        } if model == "gpt-5.4"
    )));
    assert!(
        events
            .iter()
            .all(|event| !matches!(event, AppEvent::ApplyAdvancedReasoning { .. }))
    );
}

#[tokio::test]
async fn model_reasoning_selection_popup_extra_high_warning_snapshot() {
    let (mut chat, _rx, _op_rx) = make_chatwidget_manual(Some("gpt-5.2")).await;

    set_chatgpt_auth(&mut chat);
    chat.set_reasoning_effort(Some(ReasoningEffortConfig::XHigh));

    let preset = get_available_model(&chat, "gpt-5.2");
    chat.open_reasoning_popup(preset);

    let popup = render_bottom_popup(&chat, /*width*/ 80);
    assert_chatwidget_snapshot!("model_reasoning_selection_popup_extra_high_warning", popup);
}

async fn assert_reasoning_shortcuts_update_effort(
    key_events: [KeyEvent; 2],
    expected_effort: ReasoningEffortConfig,
) {
    for key_event in key_events {
        let (mut chat, mut rx, _op_rx) = make_chatwidget_manual(Some("gpt-5.4")).await;
        chat.thread_id = Some(ThreadId::new());
        chat.set_reasoning_effort(Some(ReasoningEffortConfig::Medium));

        chat.handle_key_event(key_event);

        let events = std::iter::from_fn(|| rx.try_recv().ok()).collect::<Vec<_>>();
        assert!(
            events
                .iter()
                .all(|event| !matches!(event, AppEvent::UpdateModel(_))),
            "did not expect model update event for {key_event:?}; events: {events:?}"
        );
        assert!(
            events.iter().any(|event| matches!(
                event,
                AppEvent::UpdateReasoningEffort(Some(effort)) if effort == &expected_effort
            )),
            "expected reasoning update event for {key_event:?}; events: {events:?}"
        );
        assert!(
            events
                .iter()
                .all(|event| !matches!(event, AppEvent::PersistModelSelection { .. })),
            "expected no model persistence event for {key_event:?}; events: {events:?}"
        );
    }
}

#[tokio::test]
async fn reasoning_up_shortcuts_raise_reasoning_effort() {
    assert_reasoning_shortcuts_update_effort(
        [
            KeyEvent::new(KeyCode::Char('.'), KeyModifiers::ALT),
            KeyEvent::new(KeyCode::Up, KeyModifiers::SHIFT),
        ],
        ReasoningEffortConfig::High,
    )
    .await;
}

#[tokio::test]
async fn reasoning_down_shortcuts_lower_reasoning_effort() {
    assert_reasoning_shortcuts_update_effort(
        [
            KeyEvent::new(KeyCode::Char(','), KeyModifiers::ALT),
            KeyEvent::new(KeyCode::Down, KeyModifiers::SHIFT),
        ],
        ReasoningEffortConfig::Low,
    )
    .await;
}

#[tokio::test]
async fn reasoning_shortcut_clears_armed_quit_shortcut() {
    let (mut chat, mut rx, _op_rx) = make_chatwidget_manual(Some("gpt-5.4")).await;
    chat.thread_id = Some(ThreadId::new());
    chat.set_reasoning_effort(Some(ReasoningEffortConfig::Medium));
    chat.arm_quit_shortcut(key_hint::ctrl(KeyCode::Char('c')));

    chat.handle_key_event(KeyEvent::new(KeyCode::Char('.'), KeyModifiers::ALT));

    assert!(!chat.bottom_pane.quit_shortcut_hint_visible());
    assert!(chat.quit_shortcut_expires_at.is_none());
    assert!(chat.quit_shortcut_key.is_none());
    let events = std::iter::from_fn(|| rx.try_recv().ok()).collect::<Vec<_>>();
    assert!(
        events
            .iter()
            .all(|event| !matches!(event, AppEvent::Exit(_))),
        "did not expect reasoning shortcut to quit; events: {events:?}"
    );
}

#[tokio::test]
async fn reasoning_shortcut_is_ignored_with_model_popup_open() {
    let (mut chat, mut rx, _op_rx) = make_chatwidget_manual(Some("gpt-5.4")).await;
    chat.thread_id = Some(ThreadId::new());
    chat.set_reasoning_effort(Some(ReasoningEffortConfig::Medium));
    chat.open_model_popup();

    chat.handle_key_event(KeyEvent::new(KeyCode::Char('.'), KeyModifiers::ALT));

    let events = std::iter::from_fn(|| rx.try_recv().ok()).collect::<Vec<_>>();
    assert!(
        events
            .iter()
            .all(|event| !matches!(event, AppEvent::UpdateReasoningEffort(_))),
        "did not expect reasoning update while popup is active; events: {events:?}"
    );
    assert!(
        events
            .iter()
            .all(|event| !matches!(event, AppEvent::PersistModelSelection { .. })),
        "did not expect model persistence while popup is active; events: {events:?}"
    );
}

#[tokio::test]
async fn reasoning_up_shortcut_does_not_silently_enter_advanced_effort() {
    for (model, model_path) in [
        ("gpt-5.4", "All models → gpt-5.4"),
        ("codex-auto-test", "codex-auto-test"),
    ] {
        let (mut chat, mut rx, _op_rx) = make_chatwidget_manual(Some("gpt-5.4")).await;
        chat.thread_id = Some(ThreadId::new());
        let mut preset = get_available_model(&chat, "gpt-5.4");
        preset.id = model.to_string();
        preset.model = model.to_string();
        preset.display_name = model.to_string();
        preset.supported_reasoning_efforts.extend([
            ReasoningEffortPreset {
                effort: ReasoningEffortConfig::Max,
                description: "Maximum reasoning".to_string(),
            },
            ReasoningEffortPreset {
                effort: ReasoningEffortConfig::Ultra,
                description: "Ultra reasoning".to_string(),
            },
        ]);
        chat.model_catalog = std::sync::Arc::new(ModelCatalog::new(vec![preset]));
        chat.set_model(model);

        for effort in [ReasoningEffortConfig::XHigh, ReasoningEffortConfig::Max] {
            chat.set_reasoning_effort(Some(effort));
            chat.handle_key_event(KeyEvent::new(KeyCode::Char('.'), KeyModifiers::ALT));

            let events = std::iter::from_fn(|| rx.try_recv().ok()).collect::<Vec<_>>();
            assert!(events.iter().all(|event| !matches!(
                event,
                AppEvent::UpdateReasoningEffort(_) | AppEvent::ApplyAdvancedReasoning { .. }
            )));
            let messages = events
                .into_iter()
                .filter_map(|event| match event {
                    AppEvent::InsertHistoryCell(cell) => {
                        Some(lines_to_single_string(&cell.display_lines(/*width*/ 140)))
                    }
                    _ => None,
                })
                .collect::<Vec<_>>();
            assert_eq!(
                messages,
                vec![format!(
                    "• Max and Ultra are available under /model → {model_path} → More reasoning…\n"
                )]
            );
        }
    }
}

#[tokio::test]
async fn reasoning_down_shortcut_can_leave_advanced_effort() {
    let (mut chat, mut rx, _op_rx) = make_chatwidget_manual(Some("gpt-5.4")).await;
    chat.thread_id = Some(ThreadId::new());
    let mut preset = get_available_model(&chat, "gpt-5.4");
    preset.supported_reasoning_efforts.extend([
        ReasoningEffortPreset {
            effort: ReasoningEffortConfig::Ultra,
            description: "Ultra reasoning".to_string(),
        },
        ReasoningEffortPreset {
            effort: ReasoningEffortConfig::Max,
            description: "Maximum reasoning".to_string(),
        },
    ]);
    chat.model_catalog = std::sync::Arc::new(ModelCatalog::new(vec![preset]));

    for (current, expected) in [
        (ReasoningEffortConfig::Ultra, ReasoningEffortConfig::Max),
        (ReasoningEffortConfig::Max, ReasoningEffortConfig::XHigh),
    ] {
        chat.set_reasoning_effort(Some(current));
        chat.handle_key_event(KeyEvent::new(KeyCode::Char(','), KeyModifiers::ALT));

        let events = std::iter::from_fn(|| rx.try_recv().ok()).collect::<Vec<_>>();
        assert!(events.iter().any(|event| matches!(
            event,
            AppEvent::UpdateReasoningEffort(Some(effort)) if effort == &expected
        )));
        assert!(
            events
                .iter()
                .all(|event| !matches!(event, AppEvent::PersistModelSelection { .. }))
        );
    }
}

#[tokio::test]
async fn reasoning_popup_shows_extra_high_with_space() {
    let (mut chat, _rx, _op_rx) = make_chatwidget_manual(Some("gpt-5.4")).await;

    set_chatgpt_auth(&mut chat);

    let preset = get_available_model(&chat, "gpt-5.4");
    chat.open_reasoning_popup(preset);

    let popup = render_bottom_popup(&chat, /*width*/ 120);
    assert!(
        popup.contains("Extra high"),
        "expected popup to include 'Extra high'; popup: {popup}"
    );
    assert!(
        !popup.contains("Extrahigh"),
        "expected popup not to include 'Extrahigh'; popup: {popup}"
    );
}

#[tokio::test]
async fn single_reasoning_option_skips_selection() {
    let (mut chat, mut rx, _op_rx) = make_chatwidget_manual(/*model_override*/ None).await;

    let single_effort = vec![ReasoningEffortPreset {
        effort: ReasoningEffortConfig::High,
        description: "Greater reasoning depth for complex or ambiguous problems".to_string(),
    }];
    let preset = ModelPreset {
        id: "model-with-single-reasoning".to_string(),
        model: "model-with-single-reasoning".to_string(),
        display_name: "model-with-single-reasoning".to_string(),
        description: "".to_string(),
        model_specialty: None,
        default_reasoning_effort: ReasoningEffortConfig::High,
        supported_reasoning_efforts: single_effort,
        supports_personality: false,
        additional_speed_tiers: Vec::new(),
        service_tiers: Vec::new(),
        default_service_tier: None,
        is_default: false,
        upgrade: None,
        show_in_picker: true,
        multi_agent_version: None,
        availability_nux: None,
        supported_in_api: true,
        input_modalities: default_input_modalities(),
    };
    chat.open_reasoning_popup(preset);

    let popup = render_bottom_popup(&chat, /*width*/ 80);
    assert!(
        !popup.contains("Select Reasoning Level"),
        "expected reasoning selection popup to be skipped"
    );

    let mut events = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        events.push(ev);
    }

    assert!(
        events
            .iter()
            .any(|ev| matches!(ev, AppEvent::UpdateReasoningEffort(Some(effort)) if *effort == ReasoningEffortConfig::High)),
        "expected reasoning effort to be applied automatically; events: {events:?}"
    );
}

#[tokio::test]
async fn advanced_only_reasoning_option_requires_explicit_selection() {
    let (mut chat, mut rx, _op_rx) = make_chatwidget_manual(/*model_override*/ None).await;
    let mut preset = get_available_model(&chat, "gpt-5.4");
    preset.default_reasoning_effort = ReasoningEffortConfig::Ultra;
    preset.supported_reasoning_efforts = vec![ReasoningEffortPreset {
        effort: ReasoningEffortConfig::Ultra,
        description: "Ultra reasoning".to_string(),
    }];
    chat.open_reasoning_popup(preset);

    let popup = render_bottom_popup(&chat, /*width*/ 80);
    assert_chatwidget_snapshot!("advanced_only_reasoning_selection_popup", popup);
    let events = std::iter::from_fn(|| rx.try_recv().ok()).collect::<Vec<_>>();
    assert!(events.iter().all(|event| !matches!(
        event,
        AppEvent::UpdateReasoningEffort(_)
            | AppEvent::ApplyAdvancedReasoning { .. }
            | AppEvent::PersistModelSelection { .. }
    )));
}

#[tokio::test]
async fn auto_model_advertising_advanced_effort_opens_reasoning_picker() {
    let (mut chat, mut rx, _op_rx) = make_chatwidget_manual(/*model_override*/ None).await;
    let mut preset = get_available_model(&chat, "gpt-5.6-terra");
    preset.id = "codex-auto-test".to_string();
    preset.model = "codex-auto-test".to_string();
    preset.display_name = "codex-auto-test".to_string();
    preset.default_reasoning_effort = ReasoningEffortConfig::Medium;
    preset
        .supported_reasoning_efforts
        .push(ReasoningEffortPreset {
            effort: ReasoningEffortConfig::Ultra,
            description: "Ultra reasoning".to_string(),
        });
    chat.open_model_popup_with_presets(vec![preset]);

    chat.handle_key_event(KeyEvent::from(KeyCode::Enter));

    let events = std::iter::from_fn(|| rx.try_recv().ok()).collect::<Vec<_>>();
    assert!(events.iter().all(|event| !matches!(
        event,
        AppEvent::UpdateReasoningEffort(_)
            | AppEvent::ApplyAdvancedReasoning { .. }
            | AppEvent::PersistModelSelection { .. }
    )));
    assert!(
        events
            .iter()
            .any(|event| matches!(event, AppEvent::OpenReasoningPopup { .. }))
    );
}

#[tokio::test]
async fn feedback_selection_popup_snapshot() {
    let (mut chat, _rx, _op_rx) = make_chatwidget_manual(/*model_override*/ None).await;

    // Open the feedback category selection popup via slash command.
    chat.dispatch_command(SlashCommand::Feedback);

    let popup = render_bottom_popup(&chat, /*width*/ 80);
    assert_chatwidget_snapshot!("feedback_selection_popup", popup);
}

#[tokio::test]
async fn feedback_upload_consent_popup_snapshot() {
    let (mut chat, _rx, _op_rx) = make_chatwidget_manual(/*model_override*/ None).await;

    chat.show_selection_view(crate::bottom_pane::feedback_upload_consent_params(
        chat.app_event_tx.clone(),
        crate::app_event::FeedbackCategory::Bug,
        chat.current_rollout_path.clone(),
        Some("auto-review-rollout-thread-1.jsonl".to_string()),
        &codex_feedback::FeedbackDiagnostics::new(vec![codex_feedback::FeedbackDiagnostic {
            headline: "Proxy environment variables are set and may affect connectivity."
                .to_string(),
            details: vec!["HTTPS_PROXY = hello".to_string()],
        }]),
    ));

    let popup = render_bottom_popup(&chat, /*width*/ 80);
    assert_chatwidget_snapshot!("feedback_upload_consent_popup", popup);
}

#[tokio::test]
async fn feedback_good_result_consent_popup_includes_connectivity_diagnostics_filename() {
    let (mut chat, _rx, _op_rx) = make_chatwidget_manual(/*model_override*/ None).await;

    chat.show_selection_view(crate::bottom_pane::feedback_upload_consent_params(
        chat.app_event_tx.clone(),
        crate::app_event::FeedbackCategory::GoodResult,
        chat.current_rollout_path.clone(),
        Some("auto-review-rollout-thread-1.jsonl".to_string()),
        &codex_feedback::FeedbackDiagnostics::new(vec![codex_feedback::FeedbackDiagnostic {
            headline: "Proxy environment variables are set and may affect connectivity."
                .to_string(),
            details: vec!["HTTPS_PROXY = hello".to_string()],
        }]),
    ));

    let popup = render_bottom_popup(&chat, /*width*/ 80);
    assert_chatwidget_snapshot!("feedback_good_result_consent_popup", popup);
}

#[tokio::test]
async fn reasoning_popup_escape_returns_to_model_popup() {
    let (mut chat, _rx, _op_rx) = make_chatwidget_manual(Some("gpt-5.4")).await;
    chat.thread_id = Some(ThreadId::new());
    chat.open_model_popup();

    let preset = get_available_model(&chat, "gpt-5.4");
    chat.open_reasoning_popup(preset);

    let before_escape = render_bottom_popup(&chat, /*width*/ 80);
    assert!(before_escape.contains("Select Reasoning Level"));

    chat.handle_key_event(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));

    let after_escape = render_bottom_popup(&chat, /*width*/ 80);
    assert!(after_escape.contains("Select Model"));
    assert!(!after_escape.contains("Select Reasoning Level"));
}
