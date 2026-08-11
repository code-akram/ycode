//! Canonical TUI session state shared across cli-runtime routing, chat display, and status UI.
//!
//! The cli-runtime API is the boundary for session lifecycle events. Once those responses enter
//! TUI, this module holds the small internal state shape used by app orchestration and widgets.

use std::path::PathBuf;

use codex_protocol::ThreadId;
use codex_protocol::config_types::AgentSettings;
use codex_protocol::config_types::Personality;
use codex_utils_absolute_path::AbsolutePathBuf;
use codex_utils_path_uri::PathUri;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct MessageHistoryMetadata {
    pub(crate) log_id: u64,
    pub(crate) entry_count: usize,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ThreadSessionState {
    pub(crate) thread_id: ThreadId,
    pub(crate) forked_from_id: Option<ThreadId>,
    pub(crate) fork_parent_title: Option<String>,
    pub(crate) thread_name: Option<String>,
    pub(crate) model: String,
    pub(crate) model_provider_id: String,
    pub(crate) service_tier: Option<String>,
    pub(crate) cwd: AbsolutePathBuf,
    pub(crate) runtime_workspace_roots: Vec<AbsolutePathBuf>,
    pub(crate) instruction_source_paths: Vec<PathUri>,
    pub(crate) reasoning_effort: Option<codex_protocol::openai_models::ReasoningEffort>,
    pub(crate) agent_settings: Option<Box<AgentSettings>>,
    pub(crate) personality: Option<Personality>,
    pub(crate) message_history: Option<MessageHistoryMetadata>,
    pub(crate) rollout_path: Option<PathBuf>,
}

impl ThreadSessionState {
    pub(crate) fn set_cwd_retargeting_implicit_runtime_workspace_root(
        &mut self,
        cwd: AbsolutePathBuf,
    ) {
        let previous_cwd = std::mem::replace(&mut self.cwd, cwd.clone());
        if !self.runtime_workspace_roots.contains(&previous_cwd) {
            return;
        }

        let previous_roots = std::mem::take(&mut self.runtime_workspace_roots);
        self.runtime_workspace_roots.push(cwd);
        for root in previous_roots {
            if root != previous_cwd && !self.runtime_workspace_roots.contains(&root) {
                self.runtime_workspace_roots.push(root);
            }
        }
    }
}
