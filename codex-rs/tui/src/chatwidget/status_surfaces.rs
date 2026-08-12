//! Fixed built-in status-line and terminal-title rendering.

use super::*;
use codex_config::ConfigLayerSource;

use super::status_state::TerminalTitleStatusKind;

const TERMINAL_TITLE_ACTION_REQUIRED_PREFIX: &str = "[ ! ] Action Required";

#[derive(Clone, Debug)]
pub(super) struct CachedProjectRootName {
    pub(super) cwd: PathBuf,
    pub(super) root_name: Option<String>,
}

impl ChatWidget {
    fn status_line_cwd(&self) -> &Path {
        self.current_cwd
            .as_deref()
            .unwrap_or(self.config.cwd.as_path())
    }

    fn project_root_for_cwd(&self, cwd: &Path) -> Option<PathBuf> {
        if let Some(repo_root) = get_git_repo_root(cwd) {
            return Some(repo_root);
        }

        self.config
            .config_layer_stack
            .all_layers_low_to_high()
            .find_map(|layer| match &layer.name {
                ConfigLayerSource::Project { dot_codex_folder } => {
                    dot_codex_folder.as_path().parent().map(Path::to_path_buf)
                }
                _ => None,
            })
    }

    fn project_root_name(&mut self) -> Option<String> {
        let cwd = self.status_line_cwd().to_path_buf();
        if let Some(cache) = &self.status_line_project_root_name_cache
            && cache.cwd == cwd
        {
            return cache.root_name.clone();
        }

        let root_name = self.project_root_for_cwd(&cwd).map(|root| {
            root.file_name()
                .map(|name| name.to_string_lossy().to_string())
                .unwrap_or_else(|| format_directory_display(&root, /*max_width*/ None))
        });
        self.status_line_project_root_name_cache = Some(CachedProjectRootName {
            cwd,
            root_name: root_name.clone(),
        });
        root_name
    }

    fn terminal_title_project_name(&mut self) -> String {
        let project = self.project_root_name().unwrap_or_else(|| {
            let cwd = self.status_line_cwd();
            cwd.file_name()
                .map(|name| name.to_string_lossy().to_string())
                .unwrap_or_else(|| format_directory_display(cwd, /*max_width*/ None))
        });
        truncate_title_part(project, /*max_chars*/ 24)
    }

    fn reasoning_display_name(&self) -> String {
        let effort = self.effective_reasoning_effort();
        Self::status_line_reasoning_effort_label(effort.as_ref())
    }

    fn model_with_reasoning_display_name(&self) -> String {
        let service_tier_label = self
            .current_service_tier()
            .and_then(|service_tier| {
                self.current_model_service_tier_commands()
                    .into_iter()
                    .find(|tier| tier.id == service_tier)
                    .map(|tier| tier.name)
            })
            .filter(|_| self.has_chatgpt_account)
            .map(|tier| format!(" {tier}"))
            .unwrap_or_default();
        format!(
            "{} {}{service_tier_label}",
            self.model_display_name(),
            self.reasoning_display_name()
        )
    }

    fn refresh_fixed_status_line(&mut self) {
        self.bottom_pane.set_status_line_enabled(true);
        self.set_status_line(/*status_line*/ None);
        self.bottom_pane
            .set_model_label(Some(self.model_with_reasoning_display_name()));
        self.set_status_line_hyperlink(/*url*/ None);
    }

    pub(crate) fn clear_managed_terminal_title(&mut self) -> std::io::Result<()> {
        if self.last_terminal_title.is_some() {
            clear_terminal_title()?;
            self.last_terminal_title = None;
        }
        Ok(())
    }

    fn fixed_terminal_title(&mut self) -> String {
        let project = self.terminal_title_project_name();
        if self.terminal_title_shows_action_required() {
            return format!("{TERMINAL_TITLE_ACTION_REQUIRED_PREFIX} | {project}");
        }
        if self.bottom_pane.is_task_running() {
            return format!("{} | {project}", self.run_state_status_text());
        }
        project
    }

    pub(crate) fn refresh_terminal_title(&mut self) {
        self.last_terminal_title_requires_action = self.terminal_title_shows_action_required();
        let title = self.fixed_terminal_title();
        if self.last_terminal_title.as_deref() == Some(title.as_str()) {
            return;
        }
        match set_terminal_title(&title) {
            Ok(SetTerminalTitleResult::Applied) => self.last_terminal_title = Some(title),
            Ok(SetTerminalTitleResult::NoVisibleContent) => {
                if let Err(err) = self.clear_managed_terminal_title() {
                    tracing::debug!(error = %err, "failed to clear terminal title");
                }
            }
            Err(err) => tracing::debug!(error = %err, "failed to set terminal title"),
        }
    }

    pub(crate) fn refresh_status_surfaces(&mut self) {
        self.refresh_fixed_status_line();
        self.refresh_terminal_title();
    }

    pub(super) fn terminal_title_shows_action_required(&self) -> bool {
        self.bottom_pane.terminal_title_requires_action()
    }

    pub(super) fn run_state_status_text(&self) -> String {
        if !self.bottom_pane.is_task_running() {
            return "Ready".to_string();
        }
        match self.status_state.terminal_title_status_kind {
            TerminalTitleStatusKind::Working => "Working".to_string(),
            TerminalTitleStatusKind::WaitingForBackgroundTerminal => "Waiting".to_string(),
            TerminalTitleStatusKind::Thinking => "Thinking".to_string(),
        }
    }
}

fn truncate_title_part(value: String, max_chars: usize) -> String {
    let mut graphemes = value.graphemes(true);
    let head: String = graphemes.by_ref().take(max_chars).collect();
    if graphemes.next().is_none() || max_chars <= 3 {
        return head;
    }
    let mut truncated = head.graphemes(true).take(max_chars - 3).collect::<String>();
    truncated.push_str("...");
    truncated
}
