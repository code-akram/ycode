//! Status output and setup controls for `ChatWidget`.
//!
//! Rendering details live in `status_surfaces`; this module owns the mutable
//! widget entrypoints that apply status state, open setup views, and update the
//! history-facing `/status` surface.

use super::*;

impl ChatWidget {
    /// Update the status indicator header and details.
    ///
    /// Passing `None` clears any existing details. Returns whether the visible status indicator
    /// requested a redraw.
    pub(super) fn set_status(
        &mut self,
        header: String,
        details: Option<String>,
        details_capitalization: StatusDetailsCapitalization,
        details_max_lines: usize,
    ) -> bool {
        let details = details
            .filter(|details| !details.is_empty())
            .map(|details| {
                let trimmed = details.trim_start();
                match details_capitalization {
                    StatusDetailsCapitalization::CapitalizeFirst => {
                        crate::text_formatting::capitalize_first(trimmed)
                    }
                    StatusDetailsCapitalization::Preserve => trimmed.to_string(),
                }
            });
        self.status_state.set_status(StatusIndicatorState {
            header: header.clone(),
            details: details.clone(),
            details_max_lines,
        });
        let status_indicator_updated = self.bottom_pane.update_status(
            header,
            details,
            StatusDetailsCapitalization::Preserve,
            details_max_lines,
        );
        self.refresh_terminal_title();
        status_indicator_updated
    }

    /// Convenience wrapper around [`Self::set_status`];
    /// updates the status indicator header and clears any existing details, returning whether the
    /// visible status indicator requested a redraw.
    pub(super) fn set_status_header(&mut self, header: String) -> bool {
        self.set_status(
            header,
            /*details*/ None,
            StatusDetailsCapitalization::CapitalizeFirst,
            STATUS_DETAILS_DEFAULT_MAX_LINES,
        )
    }

    /// Sets the currently rendered footer status-line value.
    pub(crate) fn set_status_line(&mut self, status_line: Option<Line<'static>>) {
        self.bottom_pane.set_status_line(status_line);
    }

    /// Sets the terminal hyperlink target for the currently rendered footer status line.
    pub(crate) fn set_status_line_hyperlink(&mut self, url: Option<String>) {
        self.bottom_pane.set_status_line_hyperlink(url);
    }

    /// Forwards the contextual active-agent label into the bottom-pane footer pipeline.
    ///
    /// `ChatWidget` stays a pass-through here so `App` remains the owner of "which thread is the
    /// user actually looking at?" and the footer stack remains a pure renderer of that decision.
    pub(crate) fn set_active_agent_label(&mut self, active_agent_label: Option<String>) {
        self.bottom_pane.set_active_agent_label(active_agent_label);
    }

    /// Recomputes footer status-line content from config and current runtime state.
    ///
    /// This method is the status-line orchestrator: it parses configured item identifiers,
    /// warns once per session about invalid items, updates whether status-line mode is enabled,
    /// schedules async git-branch lookup when needed, and renders only values that are currently
    /// available.
    ///
    /// The omission behavior is intentional. If selected items are unavailable (for example before
    /// a session id exists or before branch lookup completes), those items are skipped without
    /// placeholders so the line remains compact and stable.
    pub(crate) fn refresh_status_line(&mut self) {
        self.refresh_status_surfaces();
    }

    pub(crate) fn add_status_output(
        &mut self,
        refreshing_rate_limits: bool,
        request_id: Option<u64>,
    ) {
        let default_usage = TokenUsage::default();
        let token_info = self.token_info.as_ref();
        let total_usage = token_info
            .map(|ti| &ti.total_token_usage)
            .unwrap_or(&default_usage);
        let model = self.current_model().to_string();
        let model_default_reasoning_effort =
            self.model_catalog
                .try_list_models()
                .ok()
                .and_then(|models| {
                    models
                        .into_iter()
                        .find(|preset| preset.model == model)
                        .map(|preset| preset.default_reasoning_effort)
                });
        let reasoning_effort_override = Some(
            self.effective_reasoning_effort()
                .or_else(|| self.config.model_reasoning_effort.clone())
                .or(model_default_reasoning_effort),
        );
        let rate_limit_snapshots: Vec<RateLimitSnapshotDisplay> = self
            .rate_limit_snapshots_by_limit_id
            .values()
            .cloned()
            .collect();
        let agents_summary =
            crate::status::compose_agents_summary(&self.config, &self.instruction_source_paths);
        let (cell, handle) = crate::status::new_status_output_with_rate_limits_handle(
            &self.config,
            self.runtime_model_provider_base_url.as_deref(),
            self.remote_connection.as_ref(),
            self.status_account_display.as_ref(),
            token_info,
            total_usage,
            &self.thread_id,
            self.thread_name.clone(),
            self.forked_from,
            rate_limit_snapshots.as_slice(),
            self.plan_type,
            Local::now(),
            self.model_display_name(),
            reasoning_effort_override,
            agents_summary,
            refreshing_rate_limits,
        );
        if let Some(request_id) = request_id {
            self.refreshing_status_outputs.push((request_id, handle));
        }
        self.add_to_history(cell);
    }

    pub(crate) fn finish_status_rate_limit_refresh(
        &mut self,
        request_id: u64,
        snapshots: Vec<RateLimitSnapshot>,
    ) {
        if !self
            .refreshing_status_outputs
            .iter()
            .any(|(pending_request_id, _)| *pending_request_id == request_id)
        {
            return;
        }

        for snapshot in snapshots {
            self.on_rate_limit_snapshot(Some(snapshot));
        }

        let rate_limit_snapshots: Vec<RateLimitSnapshotDisplay> = self
            .rate_limit_snapshots_by_limit_id
            .values()
            .cloned()
            .collect();
        let now = Local::now();
        let mut remaining = Vec::with_capacity(self.refreshing_status_outputs.len());
        let mut updated_any = false;
        for (pending_request_id, handle) in self.refreshing_status_outputs.drain(..) {
            if pending_request_id == request_id {
                updated_any = true;
                handle.finish_rate_limit_refresh(rate_limit_snapshots.as_slice(), now);
            } else {
                remaining.push((pending_request_id, handle));
            }
        }
        self.refreshing_status_outputs = remaining;
        if updated_any {
            self.request_redraw();
        }
    }

    pub(super) fn status_line_reasoning_effort_label(
        effort: Option<&ReasoningEffortConfig>,
    ) -> String {
        match effort {
            None | Some(ReasoningEffortConfig::None) => "default".to_string(),
            Some(effort) => effort.as_str().to_string(),
        }
    }
}
