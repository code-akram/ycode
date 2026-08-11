use crate::config::Config;
use crate::session::session::Session;
use crate::session::turn_context::TurnContext;
use codex_extension_api::SkillInvocationInput;
use codex_extension_api::SkillInvocationKind;
use codex_utils_absolute_path::AbsolutePathBuf;
use std::collections::HashSet;
use tokio::sync::Mutex;

pub use codex_core_skills::SkillError;
pub use codex_core_skills::SkillLoadOutcome;
pub use codex_core_skills::build_skill_name_counts;
pub use codex_core_skills::config_rules;
pub use codex_core_skills::detect_implicit_skill_invocation_for_command;
pub use codex_core_skills::filter_skill_load_outcome_for_product;
pub use codex_core_skills::injection;
pub use codex_core_skills::injection::SkillInjections;
pub use codex_core_skills::injection::build_skill_injections;
pub use codex_core_skills::loader;
pub use codex_core_skills::model;
pub use codex_core_skills::remote;
pub use codex_skills::SkillMetadata;
pub use codex_skills::SkillPolicy;
pub use codex_skills::collect_explicit_skill_mentions;
pub use codex_skills_extension::HostSkillsLoadInput;
pub use codex_skills_extension::HostSkillsService;
pub use codex_skills_extension::bundled_skills_enabled_from_stack;

#[derive(Debug, Default)]
struct ImplicitSkillInvocations(Mutex<HashSet<String>>);

pub(crate) fn skills_load_input_from_config(config: &Config) -> HostSkillsLoadInput {
    HostSkillsLoadInput::new(
        config.cwd.clone(),
        config.config_layer_stack.clone(),
        config.bundled_skills_enabled(),
    )
}

pub(crate) async fn maybe_emit_implicit_skill_invocation(
    sess: &Session,
    turn_context: &TurnContext,
    command: &str,
    workdir: &AbsolutePathBuf,
) {
    let Some(candidate) = detect_implicit_skill_invocation_for_command(
        turn_context.skills_snapshot().outcome(),
        command,
        workdir,
    ) else {
        return;
    };
    let skill_scope = match candidate.scope {
        codex_protocol::protocol::SkillScope::User => "user",
        codex_protocol::protocol::SkillScope::Repo => "repo",
        codex_protocol::protocol::SkillScope::System => "system",
        codex_protocol::protocol::SkillScope::Admin => "admin",
    };
    let skill_path = candidate.path_to_skills_md.to_string_lossy();
    let skill_name = candidate.name;
    let seen_key = format!("{skill_scope}:{skill_path}:{skill_name}");
    let inserted = {
        let skill_invocations = turn_context
            .extension_data
            .get_or_init(ImplicitSkillInvocations::default);
        let mut seen_skills = skill_invocations.0.lock().await;
        seen_skills.insert(seen_key)
    };
    if !inserted {
        return;
    }
    for contributor in sess.services.extensions.skill_invocation_contributors() {
        contributor
            .on_skill_invocation(SkillInvocationInput {
                session_store: &sess.services.session_extension_data,
                thread_store: &sess.services.thread_extension_data,
                turn_store: turn_context.extension_data.as_ref(),
                turn_id: turn_context.sub_id.as_str(),
                skill_resource: skill_path.as_ref(),
                kind: SkillInvocationKind::Implicit,
            })
            .await;
    }
}
