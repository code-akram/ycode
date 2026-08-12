use super::new_status_output;
use super::new_status_output_with_rate_limits;
use super::new_status_output_with_rate_limits_handle;
use super::rate_limit_snapshot_display;
use super::rate_limits::RateLimitSnapshotDisplay;
use super::rate_limits::RateLimitWindowDisplay;
use super::rate_limits::SpendControlLimitSnapshotDisplay;
use super::rate_limits::StatusRateLimitData;
use super::rate_limits::compose_rate_limit_data_many;
use crate::history_cell::HistoryCell;
use crate::history_cell::PlainHistoryCell;
use crate::keymap::RuntimeKeymap;
use crate::legacy_core::config::Config;
use crate::legacy_core::config::ConfigBuilder;
use crate::legacy_core::config::PermissionProfileSnapshot;
use crate::pager_overlay::TranscriptOverlay;
use crate::status::StatusAccountDisplay;
use crate::status::remote_connection::RemoteConnectionStatus;
use crate::test_support::PathBufExt;
use crate::test_support::test_path_buf;
use crate::token_usage::TokenUsage;
use crate::token_usage::TokenUsageInfo;
use app_test_support::ChatGptAuthFixture;
use app_test_support::write_chatgpt_auth;
use app_test_support::write_models_cache;
use chrono::Duration as ChronoDuration;
use chrono::Local;
use chrono::TimeZone;
use chrono::Utc;
use codex_cli_protocol::AskForApproval;
use codex_cli_protocol::CreditsSnapshot;
use codex_cli_protocol::RateLimitSnapshot;
use codex_cli_protocol::RateLimitWindow;
use codex_cli_protocol::SpendControlLimitSnapshot;
use codex_config::LoaderOverrides;
use codex_models_manager::test_support::construct_model_info_offline_for_tests;
use codex_models_manager::test_support::get_model_offline_for_tests;
use codex_protocol::ThreadId;
use codex_protocol::config_types::ApprovalsReviewer;
use codex_protocol::config_types::ReasoningSummary;
use codex_protocol::models::ActivePermissionProfile;
use codex_protocol::models::BUILT_IN_PERMISSION_PROFILE_WORKSPACE;
use codex_protocol::models::ManagedFileSystemPermissions;
use codex_protocol::models::PermissionProfile;
use codex_protocol::openai_models::ReasoningEffort;
use codex_protocol::permissions::FileSystemAccessMode;
use codex_protocol::permissions::FileSystemPath;
use codex_protocol::permissions::FileSystemSandboxEntry;
use codex_protocol::permissions::FileSystemSpecialPath;
use codex_protocol::permissions::NetworkSandboxPolicy;
use codex_utils_absolute_path::AbsolutePathBuf;
use insta::assert_snapshot;
use pretty_assertions::assert_eq;
use ratatui::prelude::*;
use std::sync::Arc;
use tempfile::TempDir;
use unicode_width::UnicodeWidthStr;

#[test]
fn stale_monthly_limit_marks_fresh_rolling_snapshot_stale() {
    let now = Local::now();
    let snapshot = RateLimitSnapshotDisplay {
        limit_name: "codex".to_string(),
        captured_at: now,
        primary: Some(RateLimitWindowDisplay {
            used_percent: 20.0,
            resets_at: Some("soon".to_string()),
            window_minutes: Some(300),
        }),
        secondary: None,
        credits: None,
        individual_limit: Some(SpendControlLimitSnapshotDisplay {
            captured_at: now - ChronoDuration::minutes(20),
            percent_remaining: 68.0,
            used: "8,000".to_string(),
            limit: "25,000".to_string(),
            resets_at: Some("later".to_string()),
        }),
    };

    assert!(matches!(
        compose_rate_limit_data_many(&[snapshot], now),
        StatusRateLimitData::Stale(_)
    ));
}

async fn test_config(temp_home: &TempDir) -> Config {
    ConfigBuilder::default()
        .codex_home(temp_home.path().to_path_buf())
        .loader_overrides(LoaderOverrides::without_managed_config_for_tests())
        .build()
        .await
        .expect("load config")
}

fn set_workspace_cwd(config: &mut Config, cwd: AbsolutePathBuf) {
    config.cwd = cwd.clone();
    config.workspace_roots = vec![cwd];
}

fn test_status_account_display() -> Option<StatusAccountDisplay> {
    None
}

fn token_info_for(model_slug: &str, config: &Config, usage: &TokenUsage) -> TokenUsageInfo {
    let context_window =
        construct_model_info_offline_for_tests(model_slug, &config.to_models_manager_config())
            .context_window;
    TokenUsageInfo {
        total_token_usage: usage.clone(),
        last_token_usage: usage.clone(),
        model_context_window: context_window,
    }
}

fn render_lines(lines: &[Line<'static>]) -> Vec<String> {
    lines
        .iter()
        .map(|line| {
            line.spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>()
        })
        .collect()
}

fn sanitize_directory(lines: Vec<String>) -> Vec<String> {
    let frame_width = lines
        .iter()
        .find(|line| line.starts_with('╭'))
        .map(|line| UnicodeWidthStr::width(line.as_str()));
    lines
        .into_iter()
        .map(|line| {
            if let (Some(frame_width), Some(dir_pos), Some(pipe_idx)) =
                (frame_width, line.find("Directory: "), line.rfind('│'))
            {
                let prefix = &line[..dir_pos + "Directory: ".len()];
                let suffix = &line[pipe_idx..];
                let replacement = "[[workspace]]";
                let content_width = frame_width.saturating_sub(
                    UnicodeWidthStr::width(prefix) + UnicodeWidthStr::width(suffix),
                );
                let mut rebuilt = prefix.to_string();
                rebuilt.push_str(replacement);
                let replacement_width = UnicodeWidthStr::width(replacement);
                if content_width > replacement_width {
                    rebuilt.push_str(&" ".repeat(content_width - replacement_width));
                }
                rebuilt.push_str(suffix);
                rebuilt
            } else {
                line
            }
        })
        .collect()
}

fn buffer_to_text(buffer: &Buffer, width: u16) -> String {
    let lines = buffer
        .content
        .chunks(usize::from(width))
        .map(|row| {
            row.iter()
                .map(|cell| {
                    let symbol = cell.symbol();
                    symbol
                        .strip_prefix("\x1b]8;;")
                        .and_then(|symbol| symbol.split_once('\x07'))
                        .and_then(|(_, symbol)| symbol.strip_suffix("\x1b]8;;\x07"))
                        .unwrap_or(symbol)
                })
                .collect::<String>()
                .trim_end()
                .to_string()
        })
        .collect::<Vec<_>>();
    sanitize_directory(lines).join("\n")
}

fn reset_at_from(captured_at: &chrono::DateTime<chrono::Local>, seconds: i64) -> i64 {
    (*captured_at + ChronoDuration::seconds(seconds))
        .with_timezone(&Utc)
        .timestamp()
}

async fn status_snapshot_includes_reasoning_details() {
    let temp_home = TempDir::new().expect("temp home");
    let mut config = test_config(&temp_home).await;
    config.model = Some("gpt-5.1-codex-max".to_string());
    config.model_provider_id = "openai".to_string();
    config.model_reasoning_summary = Some(ReasoningSummary::Detailed);
    set_workspace_cwd(&mut config, test_path_buf("/workspace/tests").abs());
    config
        .permissions
        .set_permission_profile(PermissionProfile::workspace_write())
        .expect("set permission profile");

    let account_display = test_status_account_display();
    let usage = TokenUsage {
        input_tokens: 1_200,
        cached_input_tokens: 200,
        output_tokens: 900,
        reasoning_output_tokens: 150,
        total_tokens: 2_250,
    };

    let captured_at = chrono::Local
        .with_ymd_and_hms(2024, 1, 2, 3, 4, 5)
        .single()
        .expect("timestamp");
    let snapshot = RateLimitSnapshot {
        limit_id: None,
        limit_name: None,
        primary: Some(RateLimitWindow {
            used_percent: 72,
            window_duration_mins: Some(300),
            resets_at: Some(reset_at_from(&captured_at, /*seconds*/ 600)),
        }),
        secondary: Some(RateLimitWindow {
            used_percent: 45,
            window_duration_mins: Some(10080),
            resets_at: Some(reset_at_from(&captured_at, /*seconds*/ 1_200)),
        }),
        credits: None,
        individual_limit: None,
        spend_control_reached: None,
        plan_type: None,
        rate_limit_reached_type: None,
    };
    let rate_display = rate_limit_snapshot_display(&snapshot, captured_at);

    let model_slug = get_model_offline_for_tests(config.model.as_deref());
    let token_info = token_info_for(&model_slug, &config, &usage);

    let reasoning_effort_override = Some(Some(ReasoningEffort::High));
    let composite = new_status_output(
        &config,
        account_display.as_ref(),
        Some(&token_info),
        &usage,
        &None,
        /*thread_name*/ None,
        /*forked_from*/ None,
        Some(&rate_display),
        None,
        captured_at,
        &model_slug,
        reasoning_effort_override,
    );
    let mut rendered_lines = render_lines(&composite.display_lines(/*width*/ 80));
    if cfg!(windows) {
        for line in &mut rendered_lines {
            *line = line.replace('\\', "/");
        }
    }
    let sanitized = sanitize_directory(rendered_lines).join("\n");
    assert_snapshot!(sanitized);
}

#[tokio::test]
async fn status_snapshot_shows_chatgpt_plan_without_email() {
    let temp_home = TempDir::new().expect("temp home");
    write_models_cache(temp_home.path()).expect("write models cache");
    let mut config = test_config(&temp_home).await;
    config.model = Some("gpt-5.1-codex-max".to_string());
    config.model_provider_id = "openai".to_string();
    set_workspace_cwd(&mut config, test_path_buf("/workspace/tests").abs());

    write_chatgpt_auth(
        temp_home.path(),
        ChatGptAuthFixture::new("access-chatgpt").plan_type("enterprise_cbp_automation"),
    )
    .expect("write email-less ChatGPT auth");
    let mut cli_runtime = crate::start_embedded_cli_runtime_for_picker(&config)
        .await
        .expect("start embedded app server");
    let bootstrap = cli_runtime
        .bootstrap(&config)
        .await
        .expect("bootstrap app server session");
    cli_runtime.shutdown().await.expect("shut down app server");
    let account_display = bootstrap
        .status_account_display
        .expect("bootstrap should return ChatGPT account display");
    assert_eq!(
        account_display,
        StatusAccountDisplay::ChatGpt {
            email: None,
            plan: Some("Enterprise (Automation)".to_string()),
        }
    );
    let usage = TokenUsage::default();
    let captured_at = chrono::Local
        .with_ymd_and_hms(2024, 1, 2, 3, 4, 5)
        .single()
        .expect("timestamp");
    let model_slug = get_model_offline_for_tests(config.model.as_deref());

    let composite = new_status_output(
        &config,
        Some(&account_display),
        /*token_info*/ None,
        &usage,
        &None,
        /*thread_name*/ None,
        /*forked_from*/ None,
        /*rate_limits*/ None,
        None,
        captured_at,
        &model_slug,
        /*reasoning_effort_override*/ None,
    );
    let sanitized =
        sanitize_directory(render_lines(&composite.display_lines(/*width*/ 80))).join("\n");
    assert_snapshot!(sanitized);
}

#[tokio::test]
async fn status_snapshot_includes_forked_from() {
    let temp_home = TempDir::new().expect("temp home");
    let mut config = test_config(&temp_home).await;
    config.model = Some("gpt-5.1-codex-max".to_string());
    config.model_provider_id = "openai".to_string();
    set_workspace_cwd(&mut config, test_path_buf("/workspace/tests").abs());

    let account_display = test_status_account_display();
    let usage = TokenUsage {
        input_tokens: 800,
        cached_input_tokens: 0,
        output_tokens: 400,
        reasoning_output_tokens: 0,
        total_tokens: 1_200,
    };

    let captured_at = chrono::Local
        .with_ymd_and_hms(2024, 8, 9, 10, 11, 12)
        .single()
        .expect("valid time");

    let model_slug = get_model_offline_for_tests(config.model.as_deref());
    let token_info = token_info_for(&model_slug, &config, &usage);
    let session_id =
        ThreadId::from_string("0f0f3c13-6cf9-4aa4-8b80-7d49c2f1be2e").expect("session id");
    let forked_from =
        ThreadId::from_string("e9f18a88-8081-4e51-9d4e-8af5cde2d8dd").expect("forked id");

    let composite = new_status_output(
        &config,
        account_display.as_ref(),
        Some(&token_info),
        &usage,
        &Some(session_id),
        /*thread_name*/ None,
        Some(forked_from),
        /*rate_limits*/ None,
        None,
        captured_at,
        &model_slug,
        /*reasoning_effort_override*/ None,
    );
    let mut rendered_lines = render_lines(&composite.display_lines(/*width*/ 80));
    if cfg!(windows) {
        for line in &mut rendered_lines {
            *line = line.replace('\\', "/");
        }
    }
    let sanitized = sanitize_directory(rendered_lines).join("\n");
    assert_snapshot!(sanitized);
}

#[tokio::test]
async fn status_snapshot_includes_monthly_limit() {
    let temp_home = TempDir::new().expect("temp home");
    let mut config = test_config(&temp_home).await;
    config.model = Some("gpt-5.1-codex-max".to_string());
    config.model_provider_id = "openai".to_string();
    set_workspace_cwd(&mut config, test_path_buf("/workspace/tests").abs());

    let account_display = test_status_account_display();
    let usage = TokenUsage {
        input_tokens: 800,
        cached_input_tokens: 0,
        output_tokens: 400,
        reasoning_output_tokens: 0,
        total_tokens: 1_200,
    };

    let captured_at = chrono::Local
        .with_ymd_and_hms(2024, 5, 6, 7, 8, 9)
        .single()
        .expect("timestamp");
    let snapshot = RateLimitSnapshot {
        limit_id: None,
        limit_name: None,
        primary: Some(RateLimitWindow {
            used_percent: 12,
            window_duration_mins: Some(43_200),
            resets_at: Some(reset_at_from(&captured_at, /*seconds*/ 86_400)),
        }),
        secondary: None,
        credits: None,
        individual_limit: None,
        spend_control_reached: None,
        plan_type: None,
        rate_limit_reached_type: None,
    };
    let rate_display = rate_limit_snapshot_display(&snapshot, captured_at);

    let model_slug = get_model_offline_for_tests(config.model.as_deref());
    let token_info = token_info_for(&model_slug, &config, &usage);
    let composite = new_status_output(
        &config,
        account_display.as_ref(),
        Some(&token_info),
        &usage,
        &None,
        /*thread_name*/ None,
        /*forked_from*/ None,
        Some(&rate_display),
        None,
        captured_at,
        &model_slug,
        /*reasoning_effort_override*/ None,
    );
    let mut rendered_lines = render_lines(&composite.display_lines(/*width*/ 80));
    if cfg!(windows) {
        for line in &mut rendered_lines {
            *line = line.replace('\\', "/");
        }
    }
    let sanitized = sanitize_directory(rendered_lines).join("\n");
    assert_snapshot!(sanitized);
}

#[tokio::test]
async fn status_snapshot_includes_enterprise_monthly_credit_limit() {
    let temp_home = TempDir::new().expect("temp home");
    let mut config = test_config(&temp_home).await;
    config.model = Some("gpt-5.1-codex-max".to_string());
    config.model_provider_id = "openai".to_string();
    set_workspace_cwd(&mut config, test_path_buf("/workspace/tests").abs());

    let account_display = test_status_account_display();
    let usage = TokenUsage {
        input_tokens: 800,
        cached_input_tokens: 0,
        output_tokens: 400,
        reasoning_output_tokens: 0,
        total_tokens: 1_200,
    };
    let captured_at = chrono::Local
        .with_ymd_and_hms(2024, 5, 6, 7, 8, 9)
        .single()
        .expect("timestamp");
    let snapshot = RateLimitSnapshot {
        limit_id: None,
        limit_name: None,
        primary: None,
        secondary: None,
        credits: None,
        individual_limit: Some(SpendControlLimitSnapshot {
            limit: "25000".to_string(),
            used: "8000".to_string(),
            remaining_percent: 68,
            resets_at: reset_at_from(&captured_at, /*seconds*/ 86_400),
        }),
        spend_control_reached: None,
        plan_type: None,
        rate_limit_reached_type: None,
    };
    let rate_display = rate_limit_snapshot_display(&snapshot, captured_at);

    let model_slug = get_model_offline_for_tests(config.model.as_deref());
    let token_info = token_info_for(&model_slug, &config, &usage);
    let composite = new_status_output(
        &config,
        account_display.as_ref(),
        Some(&token_info),
        &usage,
        &None,
        /*thread_name*/ None,
        /*forked_from*/ None,
        Some(&rate_display),
        None,
        captured_at,
        &model_slug,
        /*reasoning_effort_override*/ None,
    );
    let mut rendered_lines = render_lines(&composite.display_lines(/*width*/ 92));
    if cfg!(windows) {
        for line in &mut rendered_lines {
            *line = line.replace('\\', "/");
        }
    }
    let sanitized = sanitize_directory(rendered_lines).join("\n");
    assert_snapshot!(sanitized);

    let mut rendered_lines = render_lines(&composite.display_lines(/*width*/ 46));
    if cfg!(windows) {
        for line in &mut rendered_lines {
            *line = line.replace('\\', "/");
        }
    }
    let sanitized = sanitize_directory(rendered_lines).join("\n");
    assert_snapshot!(
        "status_snapshot_wraps_enterprise_monthly_credit_details_in_narrow_terminal",
        sanitized
    );
}

#[tokio::test]
async fn status_snapshot_shows_unlimited_credits() {
    let temp_home = TempDir::new().expect("temp home");
    let config = test_config(&temp_home).await;
    let account_display = test_status_account_display();
    let usage = TokenUsage::default();
    let captured_at = chrono::Local
        .with_ymd_and_hms(2024, 2, 3, 4, 5, 6)
        .single()
        .expect("timestamp");
    let snapshot = RateLimitSnapshot {
        limit_id: None,
        limit_name: None,
        primary: None,
        secondary: None,
        credits: Some(CreditsSnapshot {
            has_credits: true,
            unlimited: true,
            balance: None,
        }),
        individual_limit: None,
        spend_control_reached: None,
        plan_type: None,
        rate_limit_reached_type: None,
    };
    let rate_display = rate_limit_snapshot_display(&snapshot, captured_at);
    let model_slug = get_model_offline_for_tests(config.model.as_deref());
    let token_info = token_info_for(&model_slug, &config, &usage);
    let composite = new_status_output(
        &config,
        account_display.as_ref(),
        Some(&token_info),
        &usage,
        &None,
        /*thread_name*/ None,
        /*forked_from*/ None,
        Some(&rate_display),
        None,
        captured_at,
        &model_slug,
        /*reasoning_effort_override*/ None,
    );
    let rendered = render_lines(&composite.display_lines(/*width*/ 120));
    assert!(
        rendered
            .iter()
            .any(|line| line.contains("Credits:") && line.contains("Unlimited")),
        "expected Credits: Unlimited line, got {rendered:?}"
    );
}

#[tokio::test]
async fn status_snapshot_shows_positive_credits() {
    let temp_home = TempDir::new().expect("temp home");
    let config = test_config(&temp_home).await;
    let account_display = test_status_account_display();
    let usage = TokenUsage::default();
    let captured_at = chrono::Local
        .with_ymd_and_hms(2024, 3, 4, 5, 6, 7)
        .single()
        .expect("timestamp");
    let snapshot = RateLimitSnapshot {
        limit_id: None,
        limit_name: None,
        primary: None,
        secondary: None,
        credits: Some(CreditsSnapshot {
            has_credits: true,
            unlimited: false,
            balance: Some("12.5".to_string()),
        }),
        individual_limit: None,
        spend_control_reached: None,
        plan_type: None,
        rate_limit_reached_type: None,
    };
    let rate_display = rate_limit_snapshot_display(&snapshot, captured_at);
    let model_slug = get_model_offline_for_tests(config.model.as_deref());
    let token_info = token_info_for(&model_slug, &config, &usage);
    let composite = new_status_output(
        &config,
        account_display.as_ref(),
        Some(&token_info),
        &usage,
        &None,
        /*thread_name*/ None,
        /*forked_from*/ None,
        Some(&rate_display),
        None,
        captured_at,
        &model_slug,
        /*reasoning_effort_override*/ None,
    );
    let rendered = render_lines(&composite.display_lines(/*width*/ 120));
    assert!(
        rendered
            .iter()
            .any(|line| line.contains("Credits:") && line.contains("13 credits")),
        "expected Credits line with rounded credits, got {rendered:?}"
    );
}

#[tokio::test]
async fn status_snapshot_shows_available_credits_without_display_balance() {
    let temp_home = TempDir::new().expect("temp home");
    let config = test_config(&temp_home).await;
    let account_display = test_status_account_display();
    let usage = TokenUsage::default();
    let captured_at = chrono::Local
        .with_ymd_and_hms(2024, 4, 5, 6, 7, 8)
        .single()
        .expect("timestamp");
    let model_slug = get_model_offline_for_tests(config.model.as_deref());
    let token_info = token_info_for(&model_slug, &config, &usage);
    for balance in [
        None,
        Some(String::new()),
        Some("0".to_string()),
        Some("not-a-number".to_string()),
        Some("inf".to_string()),
    ] {
        let snapshot = RateLimitSnapshot {
            limit_id: None,
            limit_name: None,
            primary: None,
            secondary: None,
            credits: Some(CreditsSnapshot {
                has_credits: true,
                unlimited: false,
                balance,
            }),
            individual_limit: None,
            spend_control_reached: None,
            plan_type: None,
            rate_limit_reached_type: None,
        };
        let rate_display = rate_limit_snapshot_display(&snapshot, captured_at);
        let composite = new_status_output(
            &config,
            account_display.as_ref(),
            Some(&token_info),
            &usage,
            &None,
            /*thread_name*/ None,
            /*forked_from*/ None,
            Some(&rate_display),
            None,
            captured_at,
            &model_slug,
            /*reasoning_effort_override*/ None,
        );
        let rendered = render_lines(&composite.display_lines(/*width*/ 120));
        assert!(
            rendered
                .iter()
                .any(|line| line.contains("Credits:") && line.contains("Available")),
            "expected Credits: Available line, got {rendered:?}"
        );
    }
}

#[tokio::test]
async fn status_snapshot_respects_unlimited_without_has_credits_flag() {
    let temp_home = TempDir::new().expect("temp home");
    let config = test_config(&temp_home).await;
    let account_display = test_status_account_display();
    let usage = TokenUsage::default();
    let captured_at = chrono::Local
        .with_ymd_and_hms(2024, 5, 6, 7, 8, 9)
        .single()
        .expect("timestamp");
    let snapshot = RateLimitSnapshot {
        limit_id: None,
        limit_name: None,
        primary: None,
        secondary: None,
        credits: Some(CreditsSnapshot {
            has_credits: false,
            unlimited: true,
            balance: None,
        }),
        individual_limit: None,
        spend_control_reached: None,
        plan_type: None,
        rate_limit_reached_type: None,
    };
    let rate_display = rate_limit_snapshot_display(&snapshot, captured_at);
    let model_slug = get_model_offline_for_tests(config.model.as_deref());
    let token_info = token_info_for(&model_slug, &config, &usage);
    let composite = new_status_output(
        &config,
        account_display.as_ref(),
        Some(&token_info),
        &usage,
        &None,
        /*thread_name*/ None,
        /*forked_from*/ None,
        Some(&rate_display),
        None,
        captured_at,
        &model_slug,
        /*reasoning_effort_override*/ None,
    );
    let rendered = render_lines(&composite.display_lines(/*width*/ 120));
    assert!(
        rendered
            .iter()
            .any(|line| line.contains("Credits:") && line.contains("Unlimited")),
        "expected Credits: Unlimited line, got {rendered:?}"
    );
}

#[tokio::test]
async fn status_card_token_usage_excludes_cached_tokens() {
    let temp_home = TempDir::new().expect("temp home");
    let mut config = test_config(&temp_home).await;
    config.model = Some("gpt-5.1-codex-max".to_string());
    set_workspace_cwd(&mut config, test_path_buf("/workspace/tests").abs());

    let account_display = test_status_account_display();
    let usage = TokenUsage {
        input_tokens: 1_200,
        cached_input_tokens: 200,
        output_tokens: 900,
        reasoning_output_tokens: 0,
        total_tokens: 2_100,
    };

    let now = chrono::Local
        .with_ymd_and_hms(2024, 1, 1, 0, 0, 0)
        .single()
        .expect("timestamp");

    let model_slug = get_model_offline_for_tests(config.model.as_deref());
    let token_info = token_info_for(&model_slug, &config, &usage);
    let composite = new_status_output(
        &config,
        account_display.as_ref(),
        Some(&token_info),
        &usage,
        &None,
        /*thread_name*/ None,
        /*forked_from*/ None,
        /*rate_limits*/ None,
        None,
        now,
        &model_slug,
        /*reasoning_effort_override*/ None,
    );
    let rendered = render_lines(&composite.display_lines(/*width*/ 120));

    assert!(
        rendered.iter().all(|line| !line.contains("cached")),
        "cached tokens should not be displayed, got: {rendered:?}"
    );
}

#[tokio::test]
async fn status_snapshot_truncates_in_narrow_terminal() {
    let temp_home = TempDir::new().expect("temp home");
    let mut config = test_config(&temp_home).await;
    config.model = Some("gpt-5.1-codex-max".to_string());
    config.model_provider_id = "openai".to_string();
    config.model_reasoning_summary = Some(ReasoningSummary::Detailed);
    set_workspace_cwd(&mut config, test_path_buf("/workspace/tests").abs());

    let account_display = test_status_account_display();
    let usage = TokenUsage {
        input_tokens: 1_200,
        cached_input_tokens: 200,
        output_tokens: 900,
        reasoning_output_tokens: 150,
        total_tokens: 2_250,
    };

    let captured_at = chrono::Local
        .with_ymd_and_hms(2024, 1, 2, 3, 4, 5)
        .single()
        .expect("timestamp");
    let snapshot = RateLimitSnapshot {
        limit_id: None,
        limit_name: None,
        primary: Some(RateLimitWindow {
            used_percent: 72,
            window_duration_mins: Some(300),
            resets_at: Some(reset_at_from(&captured_at, /*seconds*/ 600)),
        }),
        secondary: None,
        credits: None,
        individual_limit: None,
        spend_control_reached: None,
        plan_type: None,
        rate_limit_reached_type: None,
    };
    let rate_display = rate_limit_snapshot_display(&snapshot, captured_at);

    let model_slug = get_model_offline_for_tests(config.model.as_deref());
    let token_info = token_info_for(&model_slug, &config, &usage);
    let reasoning_effort_override = Some(Some(ReasoningEffort::High));
    let composite = new_status_output(
        &config,
        account_display.as_ref(),
        Some(&token_info),
        &usage,
        &None,
        /*thread_name*/ None,
        /*forked_from*/ None,
        Some(&rate_display),
        None,
        captured_at,
        &model_slug,
        reasoning_effort_override,
    );
    let mut rendered_lines = render_lines(&composite.display_lines(/*width*/ 70));
    if cfg!(windows) {
        for line in &mut rendered_lines {
            *line = line.replace('\\', "/");
        }
    }
    let sanitized = sanitize_directory(rendered_lines).join("\n");

    assert_snapshot!(sanitized);
}

#[tokio::test]
async fn status_snapshot_truncates_halfwidth_kana_in_narrow_terminal() {
    let temp_home = TempDir::new().expect("temp home");
    let mut config = test_config(&temp_home).await;
    set_workspace_cwd(&mut config, test_path_buf("/workspace/tests").abs());

    let account = StatusAccountDisplay::ChatGpt {
        email: Some("ｶﾞﾊﾟｶﾞﾊﾟｶﾞﾊﾟ@example.com".to_string()),
        plan: Some("ｶﾞﾊﾟ plan".to_string()),
    };
    let usage = TokenUsage::default();
    let now = chrono::Local
        .with_ymd_and_hms(2024, 1, 2, 3, 4, 5)
        .single()
        .expect("timestamp");
    let composite = new_status_output(
        &config,
        Some(&account),
        /*token_info*/ None,
        &usage,
        &None,
        Some("ｶﾞﾊﾟｶﾞﾊﾟｶﾞﾊﾟｶﾞﾊﾟ thread".to_string()),
        /*forked_from*/ None,
        /*rate_limits*/ None,
        /*plan_type*/ None,
        now,
        "ｶﾞﾊﾟｶﾞﾊﾟｶﾞﾊﾟｶﾞﾊﾟ-model",
        /*reasoning_effort_override*/ None,
    );
    let rendered_lines = render_lines(&composite.display_lines(/*width*/ 42));
    let sanitized = sanitize_directory(rendered_lines).join("\n");

    assert_snapshot!(sanitized);
}

#[tokio::test]
async fn status_snapshot_shows_missing_limits_message() {
    let temp_home = TempDir::new().expect("temp home");
    let mut config = test_config(&temp_home).await;
    config.model = Some("gpt-5.1-codex-max".to_string());
    set_workspace_cwd(&mut config, test_path_buf("/workspace/tests").abs());

    let account_display = test_status_account_display();
    let usage = TokenUsage {
        input_tokens: 500,
        cached_input_tokens: 0,
        output_tokens: 250,
        reasoning_output_tokens: 0,
        total_tokens: 750,
    };

    let now = chrono::Local
        .with_ymd_and_hms(2024, 2, 3, 4, 5, 6)
        .single()
        .expect("timestamp");

    let model_slug = get_model_offline_for_tests(config.model.as_deref());
    let token_info = token_info_for(&model_slug, &config, &usage);
    let composite = new_status_output(
        &config,
        account_display.as_ref(),
        Some(&token_info),
        &usage,
        &None,
        /*thread_name*/ None,
        /*forked_from*/ None,
        /*rate_limits*/ None,
        None,
        now,
        &model_slug,
        /*reasoning_effort_override*/ None,
    );
    let mut rendered_lines = render_lines(&composite.display_lines(/*width*/ 80));
    if cfg!(windows) {
        for line in &mut rendered_lines {
            *line = line.replace('\\', "/");
        }
    }
    let sanitized = sanitize_directory(rendered_lines).join("\n");
    assert_snapshot!(sanitized);
}

#[tokio::test]
async fn status_snapshot_uses_default_reasoning_when_config_empty() {
    let temp_home = TempDir::new().expect("temp home");
    let mut config = test_config(&temp_home).await;
    config.model = Some("gpt-5.1-codex-max".to_string());
    set_workspace_cwd(&mut config, test_path_buf("/workspace/tests").abs());

    let account_display = test_status_account_display();
    let usage = TokenUsage {
        input_tokens: 500,
        cached_input_tokens: 0,
        output_tokens: 250,
        reasoning_output_tokens: 0,
        total_tokens: 750,
    };

    let now = chrono::Local
        .with_ymd_and_hms(2024, 2, 3, 4, 5, 6)
        .single()
        .expect("timestamp");
    let remote_connection = RemoteConnectionStatus {
        address: "unix:///tmp/codex-home/cli-runtime-control/cli-runtime-control.sock".to_string(),
        version: "v0.133.0".to_string(),
    };

    let model_slug = get_model_offline_for_tests(config.model.as_deref());
    let token_info = token_info_for(&model_slug, &config, &usage);
    let (composite, _) = new_status_output_with_rate_limits_handle(
        &config,
        /*runtime_model_provider_base_url*/ None,
        Some(&remote_connection),
        account_display.as_ref(),
        Some(&token_info),
        &usage,
        &None,
        /*thread_name*/ None,
        /*forked_from*/ None,
        &[],
        None,
        now,
        &model_slug,
        /*reasoning_effort_override*/ Some(Some(ReasoningEffort::Medium)),
        "<none>".to_string(),
        /*refreshing_rate_limits*/ false,
    );
    let mut rendered_lines = render_lines(&composite.display_lines(/*width*/ 80));
    if cfg!(windows) {
        for line in &mut rendered_lines {
            *line = line.replace('\\', "/");
        }
    }
    let sanitized = sanitize_directory(rendered_lines).join("\n");
    assert_snapshot!(sanitized);
}

#[tokio::test]
async fn status_snapshot_shows_refreshing_limits_notice() {
    let temp_home = TempDir::new().expect("temp home");
    let mut config = test_config(&temp_home).await;
    config.model = Some("gpt-5.1-codex-max".to_string());
    set_workspace_cwd(&mut config, test_path_buf("/workspace/tests").abs());

    let usage = TokenUsage {
        input_tokens: 500,
        cached_input_tokens: 0,
        output_tokens: 250,
        reasoning_output_tokens: 0,
        total_tokens: 750,
    };
    let captured_at = chrono::Local
        .with_ymd_and_hms(2024, 6, 7, 8, 9, 10)
        .single()
        .expect("timestamp");
    let snapshot = RateLimitSnapshot {
        limit_id: None,
        limit_name: None,
        primary: Some(RateLimitWindow {
            used_percent: 45,
            window_duration_mins: Some(300),
            resets_at: Some(reset_at_from(&captured_at, /*seconds*/ 900)),
        }),
        secondary: Some(RateLimitWindow {
            used_percent: 30,
            window_duration_mins: Some(10_080),
            resets_at: Some(reset_at_from(&captured_at, /*seconds*/ 2_700)),
        }),
        credits: None,
        individual_limit: None,
        spend_control_reached: None,
        plan_type: None,
        rate_limit_reached_type: None,
    };
    let rate_display = rate_limit_snapshot_display(&snapshot, captured_at);

    let model_slug = get_model_offline_for_tests(config.model.as_deref());
    let token_info = token_info_for(&model_slug, &config, &usage);
    let composite = new_status_output_with_rate_limits(
        &config,
        /*account_display*/ None,
        Some(&token_info),
        &usage,
        &None,
        /*thread_name*/ None,
        /*forked_from*/ None,
        std::slice::from_ref(&rate_display),
        None,
        captured_at,
        &model_slug,
        /*reasoning_effort_override*/ None,
        /*refreshing_rate_limits*/ true,
    );
    let mut rendered_lines = render_lines(&composite.display_lines(/*width*/ 80));
    if cfg!(windows) {
        for line in &mut rendered_lines {
            *line = line.replace('\\', "/");
        }
    }
    let sanitized = sanitize_directory(rendered_lines).join("\n");
    assert_snapshot!(sanitized);
}

#[tokio::test]
async fn transcript_overlay_remeasures_status_after_rate_limit_refresh() {
    let temp_home = TempDir::new().expect("temp home");
    let mut config = test_config(&temp_home).await;
    config.model = Some("gpt-5.1-codex-max".to_string());
    set_workspace_cwd(&mut config, test_path_buf("/workspace/tests").abs());
    let usage = TokenUsage::default();
    let now = Local
        .with_ymd_and_hms(2024, 6, 7, 8, 9, 10)
        .single()
        .expect("timestamp");
    let model_slug = get_model_offline_for_tests(config.model.as_deref());

    let (status, handle) = new_status_output_with_rate_limits_handle(
        &config,
        /*runtime_model_provider_base_url*/ None,
        /*remote_connection*/ None,
        /*account_display*/ None,
        /*token_info*/ None,
        &usage,
        &None,
        /*thread_name*/ None,
        /*forked_from*/ None,
        /*rate_limits*/ &[],
        None,
        now,
        &model_slug,
        /*reasoning_effort_override*/ None,
        "<none>".to_string(),
        /*refreshing_rate_limits*/ true,
    );
    let mut overlay =
        TranscriptOverlay::new(vec![Arc::new(status)], RuntimeKeymap::defaults().pager);
    let area = Rect::new(
        /*x*/ 0, /*y*/ 0, /*width*/ 80, /*height*/ 30,
    );
    let mut buffer = Buffer::empty(area);
    overlay.render(area, &mut buffer);
    let before = buffer_to_text(&buffer, area.width);

    handle.finish_rate_limit_refresh(
        &[RateLimitSnapshotDisplay {
            limit_name: "spark".to_string(),
            captured_at: now,
            primary: Some(RateLimitWindowDisplay {
                used_percent: 45.0,
                resets_at: Some("soon".to_string()),
                window_minutes: Some(300),
            }),
            secondary: Some(RateLimitWindowDisplay {
                used_percent: 30.0,
                resets_at: Some("later".to_string()),
                window_minutes: Some(10_080),
            }),
            credits: None,
            individual_limit: None,
        }],
        now,
    );
    overlay.insert_cell(Arc::new(PlainHistoryCell::new(vec!["next message".into()])));
    buffer = Buffer::empty(area);
    overlay.render(area, &mut buffer);
    let after = buffer_to_text(&buffer, area.width);

    assert!(
        after.contains("spark limit"),
        "status output was clipped: {after:?}"
    );
    assert!(
        after.contains("5h limit"),
        "status output was clipped: {after:?}"
    );
    assert!(
        after.contains("Weekly limit"),
        "status output was clipped: {after:?}"
    );
    insta::assert_snapshot!(
        "transcript_overlay_status_rate_limit_refresh",
        format!("before:\n{before}\n\nafter:\n{after}")
    );
}

#[tokio::test]
async fn status_snapshot_includes_credits_and_limits() {
    let temp_home = TempDir::new().expect("temp home");
    let mut config = test_config(&temp_home).await;
    config.model = Some("gpt-5.1-codex".to_string());
    set_workspace_cwd(&mut config, test_path_buf("/workspace/tests").abs());

    let account_display = test_status_account_display();
    let usage = TokenUsage {
        input_tokens: 1_500,
        cached_input_tokens: 100,
        output_tokens: 600,
        reasoning_output_tokens: 0,
        total_tokens: 2_200,
    };

    let captured_at = chrono::Local
        .with_ymd_and_hms(2024, 7, 8, 9, 10, 11)
        .single()
        .expect("timestamp");
    let snapshot = RateLimitSnapshot {
        limit_id: None,
        limit_name: None,
        primary: Some(RateLimitWindow {
            used_percent: 45,
            window_duration_mins: Some(300),
            resets_at: Some(reset_at_from(&captured_at, /*seconds*/ 900)),
        }),
        secondary: Some(RateLimitWindow {
            used_percent: 30,
            window_duration_mins: Some(10_080),
            resets_at: Some(reset_at_from(&captured_at, /*seconds*/ 2_700)),
        }),
        credits: Some(CreditsSnapshot {
            has_credits: true,
            unlimited: false,
            balance: None,
        }),
        individual_limit: None,
        spend_control_reached: None,
        plan_type: None,
        rate_limit_reached_type: None,
    };
    let rate_display = rate_limit_snapshot_display(&snapshot, captured_at);

    let model_slug = get_model_offline_for_tests(config.model.as_deref());
    let token_info = token_info_for(&model_slug, &config, &usage);
    let composite = new_status_output(
        &config,
        account_display.as_ref(),
        Some(&token_info),
        &usage,
        &None,
        /*thread_name*/ None,
        /*forked_from*/ None,
        Some(&rate_display),
        None,
        captured_at,
        &model_slug,
        /*reasoning_effort_override*/ None,
    );
    let mut rendered_lines = render_lines(&composite.display_lines(/*width*/ 80));
    if cfg!(windows) {
        for line in &mut rendered_lines {
            *line = line.replace('\\', "/");
        }
    }
    let sanitized = sanitize_directory(rendered_lines).join("\n");
    assert_snapshot!(sanitized);
}

#[tokio::test]
async fn status_snapshot_shows_unavailable_limits_message() {
    let temp_home = TempDir::new().expect("temp home");
    let mut config = test_config(&temp_home).await;
    config.model = Some("gpt-5.1-codex-max".to_string());
    set_workspace_cwd(&mut config, test_path_buf("/workspace/tests").abs());

    let account_display = test_status_account_display();
    let usage = TokenUsage {
        input_tokens: 500,
        cached_input_tokens: 0,
        output_tokens: 250,
        reasoning_output_tokens: 0,
        total_tokens: 750,
    };

    let snapshot = RateLimitSnapshot {
        limit_id: None,
        limit_name: None,
        primary: None,
        secondary: None,
        credits: None,
        individual_limit: None,
        spend_control_reached: None,
        plan_type: None,
        rate_limit_reached_type: None,
    };
    let captured_at = chrono::Local
        .with_ymd_and_hms(2024, 6, 7, 8, 9, 10)
        .single()
        .expect("timestamp");
    let rate_display = rate_limit_snapshot_display(&snapshot, captured_at);

    let model_slug = get_model_offline_for_tests(config.model.as_deref());
    let token_info = token_info_for(&model_slug, &config, &usage);
    let composite = new_status_output(
        &config,
        account_display.as_ref(),
        Some(&token_info),
        &usage,
        &None,
        /*thread_name*/ None,
        /*forked_from*/ None,
        Some(&rate_display),
        None,
        captured_at,
        &model_slug,
        /*reasoning_effort_override*/ None,
    );
    let mut rendered_lines = render_lines(&composite.display_lines(/*width*/ 80));
    if cfg!(windows) {
        for line in &mut rendered_lines {
            *line = line.replace('\\', "/");
        }
    }
    let sanitized = sanitize_directory(rendered_lines).join("\n");
    assert_snapshot!(sanitized);
}

#[tokio::test]
async fn status_snapshot_treats_refreshing_empty_limits_as_unavailable() {
    let temp_home = TempDir::new().expect("temp home");
    let mut config = test_config(&temp_home).await;
    config.model = Some("gpt-5.1-codex-max".to_string());
    set_workspace_cwd(&mut config, test_path_buf("/workspace/tests").abs());

    let usage = TokenUsage {
        input_tokens: 500,
        cached_input_tokens: 0,
        output_tokens: 250,
        reasoning_output_tokens: 0,
        total_tokens: 750,
    };

    let snapshot = RateLimitSnapshot {
        limit_id: None,
        limit_name: None,
        primary: None,
        secondary: None,
        credits: None,
        individual_limit: None,
        spend_control_reached: None,
        plan_type: None,
        rate_limit_reached_type: None,
    };
    let captured_at = chrono::Local
        .with_ymd_and_hms(2024, 6, 7, 8, 9, 10)
        .single()
        .expect("timestamp");
    let rate_display = rate_limit_snapshot_display(&snapshot, captured_at);

    let model_slug = get_model_offline_for_tests(config.model.as_deref());
    let token_info = token_info_for(&model_slug, &config, &usage);
    let composite = new_status_output_with_rate_limits(
        &config,
        /*account_display*/ None,
        Some(&token_info),
        &usage,
        &None,
        /*thread_name*/ None,
        /*forked_from*/ None,
        std::slice::from_ref(&rate_display),
        None,
        captured_at,
        &model_slug,
        /*reasoning_effort_override*/ None,
        /*refreshing_rate_limits*/ true,
    );
    let mut rendered_lines = render_lines(&composite.display_lines(/*width*/ 80));
    if cfg!(windows) {
        for line in &mut rendered_lines {
            *line = line.replace('\\', "/");
        }
    }
    let sanitized = sanitize_directory(rendered_lines).join("\n");
    assert_snapshot!(sanitized);
}

#[tokio::test]
async fn status_snapshot_shows_stale_limits_message() {
    let temp_home = TempDir::new().expect("temp home");
    let mut config = test_config(&temp_home).await;
    config.model = Some("gpt-5.1-codex-max".to_string());
    set_workspace_cwd(&mut config, test_path_buf("/workspace/tests").abs());

    let account_display = test_status_account_display();
    let usage = TokenUsage {
        input_tokens: 1_200,
        cached_input_tokens: 200,
        output_tokens: 900,
        reasoning_output_tokens: 150,
        total_tokens: 2_250,
    };

    let captured_at = chrono::Local
        .with_ymd_and_hms(2024, 1, 2, 3, 4, 5)
        .single()
        .expect("timestamp");
    let snapshot = RateLimitSnapshot {
        limit_id: None,
        limit_name: None,
        primary: Some(RateLimitWindow {
            used_percent: 72,
            window_duration_mins: Some(300),
            resets_at: Some(reset_at_from(&captured_at, /*seconds*/ 600)),
        }),
        secondary: Some(RateLimitWindow {
            used_percent: 40,
            window_duration_mins: Some(10_080),
            resets_at: Some(reset_at_from(&captured_at, /*seconds*/ 1_800)),
        }),
        credits: None,
        individual_limit: None,
        spend_control_reached: None,
        plan_type: None,
        rate_limit_reached_type: None,
    };
    let rate_display = rate_limit_snapshot_display(&snapshot, captured_at);
    let now = captured_at + ChronoDuration::minutes(20);

    let model_slug = get_model_offline_for_tests(config.model.as_deref());
    let token_info = token_info_for(&model_slug, &config, &usage);
    let composite = new_status_output(
        &config,
        account_display.as_ref(),
        Some(&token_info),
        &usage,
        &None,
        /*thread_name*/ None,
        /*forked_from*/ None,
        Some(&rate_display),
        None,
        now,
        &model_slug,
        /*reasoning_effort_override*/ None,
    );
    let mut rendered_lines = render_lines(&composite.display_lines(/*width*/ 80));
    if cfg!(windows) {
        for line in &mut rendered_lines {
            *line = line.replace('\\', "/");
        }
    }
    let sanitized = sanitize_directory(rendered_lines).join("\n");
    assert_snapshot!(sanitized);
}

#[tokio::test]
async fn status_snapshot_cached_limits_hide_credits_without_flag() {
    let temp_home = TempDir::new().expect("temp home");
    let mut config = test_config(&temp_home).await;
    config.model = Some("gpt-5.1-codex".to_string());
    set_workspace_cwd(&mut config, test_path_buf("/workspace/tests").abs());

    let account_display = test_status_account_display();
    let usage = TokenUsage {
        input_tokens: 900,
        cached_input_tokens: 200,
        output_tokens: 350,
        reasoning_output_tokens: 0,
        total_tokens: 1_450,
    };

    let captured_at = chrono::Local
        .with_ymd_and_hms(2024, 9, 10, 11, 12, 13)
        .single()
        .expect("timestamp");
    let snapshot = RateLimitSnapshot {
        limit_id: None,
        limit_name: None,
        primary: Some(RateLimitWindow {
            used_percent: 60,
            window_duration_mins: Some(300),
            resets_at: Some(reset_at_from(&captured_at, /*seconds*/ 1_200)),
        }),
        secondary: Some(RateLimitWindow {
            used_percent: 35,
            window_duration_mins: Some(10_080),
            resets_at: Some(reset_at_from(&captured_at, /*seconds*/ 2_400)),
        }),
        credits: Some(CreditsSnapshot {
            has_credits: false,
            unlimited: false,
            balance: Some("80".to_string()),
        }),
        individual_limit: None,
        spend_control_reached: None,
        plan_type: None,
        rate_limit_reached_type: None,
    };
    let rate_display = rate_limit_snapshot_display(&snapshot, captured_at);
    let now = captured_at + ChronoDuration::minutes(20);

    let model_slug = get_model_offline_for_tests(config.model.as_deref());
    let token_info = token_info_for(&model_slug, &config, &usage);
    let composite = new_status_output(
        &config,
        account_display.as_ref(),
        Some(&token_info),
        &usage,
        &None,
        /*thread_name*/ None,
        /*forked_from*/ None,
        Some(&rate_display),
        None,
        now,
        &model_slug,
        /*reasoning_effort_override*/ None,
    );
    let mut rendered_lines = render_lines(&composite.display_lines(/*width*/ 80));
    if cfg!(windows) {
        for line in &mut rendered_lines {
            *line = line.replace('\\', "/");
        }
    }
    let sanitized = sanitize_directory(rendered_lines).join("\n");
    assert_snapshot!(sanitized);
}

#[tokio::test]
async fn status_context_window_uses_last_usage() {
    let temp_home = TempDir::new().expect("temp home");
    let mut config = test_config(&temp_home).await;
    config.model_context_window = Some(272_000);

    let account_display = test_status_account_display();
    let total_usage = TokenUsage {
        input_tokens: 12_800,
        cached_input_tokens: 0,
        output_tokens: 879,
        reasoning_output_tokens: 0,
        total_tokens: 102_000,
    };
    let last_usage = TokenUsage {
        input_tokens: 12_800,
        cached_input_tokens: 0,
        output_tokens: 879,
        reasoning_output_tokens: 0,
        total_tokens: 13_679,
    };

    let now = chrono::Local
        .with_ymd_and_hms(2024, 6, 1, 12, 0, 0)
        .single()
        .expect("timestamp");

    let model_slug = get_model_offline_for_tests(config.model.as_deref());
    let token_info = TokenUsageInfo {
        total_token_usage: total_usage.clone(),
        last_token_usage: last_usage,
        model_context_window: config.model_context_window,
    };
    let composite = new_status_output(
        &config,
        account_display.as_ref(),
        Some(&token_info),
        &total_usage,
        &None,
        /*thread_name*/ None,
        /*forked_from*/ None,
        /*rate_limits*/ None,
        None,
        now,
        &model_slug,
        /*reasoning_effort_override*/ None,
    );
    let rendered_lines = render_lines(&composite.display_lines(/*width*/ 80));
    let context_line = rendered_lines
        .into_iter()
        .find(|line| line.contains("Context window"))
        .expect("context line");

    assert!(
        context_line.contains("13.7K used / 272K"),
        "expected context line to reflect last usage tokens, got: {context_line}"
    );
    assert!(
        !context_line.contains("102K"),
        "context line should not use total aggregated tokens, got: {context_line}"
    );
}
