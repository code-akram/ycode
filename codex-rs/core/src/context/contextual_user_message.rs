use codex_protocol::models::ContentItem;

use super::AdditionalContextUserFragment;
use super::ContextualUserFragment;
use super::InternalModelContextFragment;
use super::LegacyApplyPatchExecCommandWarning;
use super::LegacyModelMismatchWarning;
use super::LegacyUnifiedExecProcessLimitWarning;
use super::SkillInstructions;
use super::SubagentNotification;
use super::TurnAborted;
use super::UserInstructions;
use super::UserShellCommand;
use super::world_state::EnvironmentsState;

const CONTEXTUAL_USER_FRAGMENT_MATCHERS: &[fn(&str) -> bool] = &[
    UserInstructions::matches_text,
    EnvironmentsState::matches_text,
    AdditionalContextUserFragment::matches_text,
    SkillInstructions::matches_text,
    UserShellCommand::matches_text,
    TurnAborted::matches_text,
    SubagentNotification::matches_text,
    InternalModelContextFragment::matches_text,
    LegacyUnifiedExecProcessLimitWarning::matches_text,
    LegacyApplyPatchExecCommandWarning::matches_text,
    LegacyModelMismatchWarning::matches_text,
];

fn is_standard_contextual_user_text(text: &str) -> bool {
    CONTEXTUAL_USER_FRAGMENT_MATCHERS
        .iter()
        .any(|matches_text| matches_text(text))
}

pub(crate) fn is_contextual_user_fragment(content_item: &ContentItem) -> bool {
    let ContentItem::InputText { text } = content_item else {
        return false;
    };
    is_standard_contextual_user_text(text)
}

#[cfg(test)]
#[path = "contextual_user_message_tests.rs"]
mod tests;
