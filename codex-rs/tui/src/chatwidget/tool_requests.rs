//! Interactive tool request surfaces for `ChatWidget`.
//!
//! This module owns user-input prompts that block on user decisions.

use super::*;

impl ChatWidget {
    pub(super) fn on_request_user_input(&mut self, ev: ToolRequestUserInputParams) {
        self.defer_or_handle(
            ev,
            InterruptManager::push_user_input,
            Self::handle_request_user_input_now,
        );
    }

    pub(crate) fn handle_request_user_input_now(&mut self, ev: ToolRequestUserInputParams) {
        self.flush_answer_stream_with_separator();
        let question_count = ev.questions.len();
        let summary = Notification::user_input_request_summary(&ev.questions);
        let title = match (question_count, summary.as_deref()) {
            (1, Some(summary)) => summary.to_string(),
            (1, None) => "Question requested".to_string(),
            (count, _) => format!("{count} questions requested"),
        };
        self.notify(Notification::UserInputPrompt { title });
        self.bottom_pane.push_user_input_request(ev);
        self.request_redraw();
    }
}
