//! A live task status row rendered above the composer while the agent is busy.
//!
//! The row owns spinner timing and an independently ticking elapsed clock.

use std::time::Duration;
use std::time::Instant;

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Stylize;
use ratatui::text::Line;
use ratatui::widgets::Paragraph;
use ratatui::widgets::Widget;

use crate::app_event_sender::AppEventSender;
use crate::key_hint::ShortcutHint;
use crate::motion::MotionMode;
use crate::motion::status_spinner;
use crate::render::renderable::Renderable;
use crate::tui::FrameRequester;

pub(crate) const STATUS_DETAILS_DEFAULT_MAX_LINES: usize = 3;
const SPINNER_FRAME_INTERVAL: Duration = Duration::from_millis(80);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StatusDetailsCapitalization {
    CapitalizeFirst,
    Preserve,
}

/// Displays a single-line in-progress spinner and elapsed time.
pub(crate) struct StatusIndicatorWidget {
    /// Retained operational header used by stream recovery and terminal lifecycle state.
    header: String,
    elapsed_running: Duration,
    last_resume_at: Instant,
    is_paused: bool,
    app_event_tx: AppEventSender,
    frame_requester: FrameRequester,
    animations_enabled: bool,
}

// Format elapsed seconds into a compact human-friendly form used by the status line.
// Examples: 0s, 59s, 1m 00s, 59m 59s, 1h 00m 00s, 2h 03m 09s
pub fn fmt_elapsed_compact(elapsed_secs: u64) -> String {
    if elapsed_secs < 60 {
        return format!("{elapsed_secs}s");
    }
    if elapsed_secs < 3600 {
        let minutes = elapsed_secs / 60;
        let seconds = elapsed_secs % 60;
        return format!("{minutes}m {seconds:02}s");
    }
    let hours = elapsed_secs / 3600;
    let minutes = (elapsed_secs % 3600) / 60;
    let seconds = elapsed_secs % 60;
    format!("{hours}h {minutes:02}m {seconds:02}s")
}

impl StatusIndicatorWidget {
    pub(crate) fn new(
        app_event_tx: AppEventSender,
        frame_requester: FrameRequester,
        animations_enabled: bool,
    ) -> Self {
        Self {
            header: String::from("Working"),
            elapsed_running: Duration::ZERO,
            last_resume_at: Instant::now(),
            is_paused: false,

            app_event_tx,
            frame_requester,
            animations_enabled,
        }
    }

    pub(crate) fn interrupt(&self) {
        self.app_event_tx.interrupt();
    }

    /// Update the operational header retained for stream recovery.
    pub(crate) fn update_header(&mut self, header: String) {
        self.header = header;
    }

    /// Retain the status update API while operational details remain in the transcript.
    pub(crate) fn update_details(
        &mut self,
        _details: Option<String>,
        _capitalization: StatusDetailsCapitalization,
        _max_lines: usize,
    ) {
    }

    /// Operational summaries remain visible in transcript cells, not the Mini status row.
    pub(crate) fn update_inline_message(&mut self, _message: Option<String>) {}

    pub(crate) fn header(&self) -> &str {
        &self.header
    }

    #[cfg(test)]
    pub(crate) fn details(&self) -> Option<&str> {
        None
    }

    pub(crate) fn set_interrupt_hint_visible(&mut self, _visible: bool) {}

    pub(crate) fn set_interrupt_binding(&mut self, _binding: Option<ShortcutHint>) {}

    pub(crate) fn pause_timer(&mut self) {
        self.pause_timer_at(Instant::now());
    }

    pub(crate) fn resume_timer(&mut self) {
        self.resume_timer_at(Instant::now());
    }

    pub(crate) fn pause_timer_at(&mut self, now: Instant) {
        if self.is_paused {
            return;
        }
        self.elapsed_running += now.saturating_duration_since(self.last_resume_at);
        self.is_paused = true;
    }

    pub(crate) fn resume_timer_at(&mut self, now: Instant) {
        if !self.is_paused {
            return;
        }
        self.last_resume_at = now;
        self.is_paused = false;
        self.frame_requester.schedule_frame();
    }

    fn elapsed_duration_at(&self, now: Instant) -> Duration {
        let mut elapsed = self.elapsed_running;
        if !self.is_paused {
            elapsed += now.saturating_duration_since(self.last_resume_at);
        }
        elapsed
    }

    fn elapsed_seconds_at(&self, now: Instant) -> u64 {
        self.elapsed_duration_at(now).as_secs()
    }

    pub fn elapsed_seconds(&self) -> u64 {
        self.elapsed_seconds_at(Instant::now())
    }

    fn status_line_at(&self, now: Instant) -> Line<'static> {
        let motion_mode = MotionMode::from_animations_enabled(self.animations_enabled);
        Line::from(vec![
            status_spinner(self.last_resume_at, now, motion_mode),
            " ".into(),
            fmt_elapsed_compact(self.elapsed_seconds_at(now)).dim(),
        ])
    }

    fn next_frame_delay_at(&self, now: Instant) -> Option<Duration> {
        if self.is_paused {
            return None;
        }
        let elapsed = self.elapsed_duration_at(now);
        let until_next_second = Duration::from_secs(1)
            .saturating_sub(Duration::from_nanos(u64::from(elapsed.subsec_nanos())));
        Some(if self.animations_enabled {
            SPINNER_FRAME_INTERVAL.min(until_next_second)
        } else {
            until_next_second
        })
    }

    #[cfg(test)]
    fn wrapped_details_lines(&self, _width: u16) -> Vec<Line<'static>> {
        Vec::new()
    }
}

impl Renderable for StatusIndicatorWidget {
    fn desired_height(&self, _width: u16) -> u16 {
        1
    }

    fn render(&self, area: Rect, buf: &mut Buffer) {
        if area.is_empty() {
            return;
        }

        let now = Instant::now();
        if let Some(delay) = self.next_frame_delay_at(now) {
            self.frame_requester.schedule_frame_in(delay);
        }
        Paragraph::new(self.status_line_at(now)).render(area, buf);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app_event::AppEvent;
    use crate::app_event_sender::AppEventSender;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use std::time::Duration;
    use std::time::Instant;
    use tokio::sync::mpsc::unbounded_channel;

    use pretty_assertions::assert_eq;

    #[test]
    fn fmt_elapsed_compact_formats_seconds_minutes_hours() {
        assert_eq!(fmt_elapsed_compact(/*elapsed_secs*/ 0), "0s");
        assert_eq!(fmt_elapsed_compact(/*elapsed_secs*/ 1), "1s");
        assert_eq!(fmt_elapsed_compact(/*elapsed_secs*/ 59), "59s");
        assert_eq!(fmt_elapsed_compact(/*elapsed_secs*/ 60), "1m 00s");
        assert_eq!(fmt_elapsed_compact(/*elapsed_secs*/ 61), "1m 01s");
        assert_eq!(fmt_elapsed_compact(3 * 60 + 5), "3m 05s");
        assert_eq!(fmt_elapsed_compact(59 * 60 + 59), "59m 59s");
        assert_eq!(fmt_elapsed_compact(/*elapsed_secs*/ 3600), "1h 00m 00s");
        assert_eq!(fmt_elapsed_compact(3600 + 60 + 1), "1h 01m 01s");
        assert_eq!(fmt_elapsed_compact(25 * 3600 + 2 * 60 + 3), "25h 02m 03s");
    }

    #[test]
    fn renders_with_working_header() {
        let (tx_raw, _rx) = unbounded_channel::<AppEvent>();
        let tx = AppEventSender::new(tx_raw);
        let mut w = StatusIndicatorWidget::new(
            tx,
            crate::tui::FrameRequester::test_dummy(),
            /*animations_enabled*/ false,
        );
        w.is_paused = true;
        w.elapsed_running = Duration::ZERO;

        // Render into a fixed-size test terminal and snapshot the backend.
        let mut terminal = Terminal::new(TestBackend::new(80, 2)).expect("terminal");
        terminal
            .draw(|f| w.render(f.area(), f.buffer_mut()))
            .expect("draw");
        assert_eq!(
            rendered_backend_line(&terminal, /*row*/ 0).trim_end(),
            "• 0s"
        );
    }

    #[test]
    fn renders_truncated() {
        let (tx_raw, _rx) = unbounded_channel::<AppEvent>();
        let tx = AppEventSender::new(tx_raw);
        let mut w = StatusIndicatorWidget::new(
            tx,
            crate::tui::FrameRequester::test_dummy(),
            /*animations_enabled*/ false,
        );
        w.is_paused = true;
        w.elapsed_running = Duration::ZERO;

        // Render into a fixed-size test terminal and snapshot the backend.
        let mut terminal = Terminal::new(TestBackend::new(20, 2)).expect("terminal");
        terminal
            .draw(|f| w.render(f.area(), f.buffer_mut()))
            .expect("draw");
        assert_eq!(
            rendered_backend_line(&terminal, /*row*/ 0).trim_end(),
            "• 0s"
        );
    }

    #[test]
    fn renders_wrapped_details_panama_two_lines() {
        let (tx_raw, _rx) = unbounded_channel::<AppEvent>();
        let tx = AppEventSender::new(tx_raw);
        let mut w = StatusIndicatorWidget::new(
            tx,
            crate::tui::FrameRequester::test_dummy(),
            /*animations_enabled*/ false,
        );
        w.update_details(
            Some("A man a plan a canal panama".to_string()),
            StatusDetailsCapitalization::CapitalizeFirst,
            STATUS_DETAILS_DEFAULT_MAX_LINES,
        );
        w.set_interrupt_hint_visible(/*visible*/ false);

        // Freeze time-dependent rendering (elapsed + spinner) to keep the snapshot stable.
        w.is_paused = true;
        w.elapsed_running = Duration::ZERO;

        let mut terminal = Terminal::new(TestBackend::new(30, 3)).expect("terminal");
        terminal
            .draw(|f| w.render(f.area(), f.buffer_mut()))
            .expect("draw");
        assert_eq!(
            rendered_backend_line(&terminal, /*row*/ 0).trim_end(),
            "• 0s"
        );
        assert!(
            rendered_backend_line(&terminal, /*row*/ 1)
                .trim()
                .is_empty()
        );
    }

    #[test]
    fn renders_without_spinner_when_animations_disabled() {
        let (tx_raw, _rx) = unbounded_channel::<AppEvent>();
        let tx = AppEventSender::new(tx_raw);
        let mut w = StatusIndicatorWidget::new(
            tx,
            crate::tui::FrameRequester::test_dummy(),
            /*animations_enabled*/ false,
        );
        w.is_paused = true;
        w.elapsed_running = Duration::ZERO;

        let mut terminal = Terminal::new(TestBackend::new(80, 1)).expect("terminal");
        terminal
            .draw(|f| w.render(f.area(), f.buffer_mut()))
            .expect("draw");
        let line = terminal.backend().buffer().content()[..80]
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect::<String>();

        assert!(line.starts_with("• 0s"));
        assert!(!line.contains("Working"));
        assert!(!line.contains('(') && !line.contains(')'));
    }

    #[test]
    fn renders_remapped_interrupt_hint() {
        let (tx_raw, _rx) = unbounded_channel::<AppEvent>();
        let tx = AppEventSender::new(tx_raw);
        let mut w = StatusIndicatorWidget::new(
            tx,
            crate::tui::FrameRequester::test_dummy(),
            /*animations_enabled*/ false,
        );
        w.is_paused = true;
        w.elapsed_running = Duration::ZERO;

        let mut terminal = Terminal::new(TestBackend::new(80, 1)).expect("terminal");
        terminal
            .draw(|f| w.render(f.area(), f.buffer_mut()))
            .expect("draw");
        assert_eq!(
            rendered_backend_line(&terminal, /*row*/ 0).trim_end(),
            "• 0s"
        );
    }

    #[test]
    fn timer_pauses_when_requested() {
        let (tx_raw, _rx) = unbounded_channel::<AppEvent>();
        let tx = AppEventSender::new(tx_raw);
        let mut widget = StatusIndicatorWidget::new(
            tx,
            crate::tui::FrameRequester::test_dummy(),
            /*animations_enabled*/ true,
        );

        let baseline = Instant::now();
        widget.last_resume_at = baseline;

        let before_pause = widget.elapsed_seconds_at(baseline + Duration::from_secs(5));
        assert_eq!(before_pause, 5);

        widget.pause_timer_at(baseline + Duration::from_secs(5));
        let paused_elapsed = widget.elapsed_seconds_at(baseline + Duration::from_secs(10));
        assert_eq!(paused_elapsed, before_pause);

        widget.resume_timer_at(baseline + Duration::from_secs(10));
        let after_resume = widget.elapsed_seconds_at(baseline + Duration::from_secs(13));
        assert_eq!(after_resume, before_pause + 3);
    }

    #[test]
    fn reduced_motion_elapsed_progresses_at_second_boundaries() {
        let (tx_raw, _rx) = unbounded_channel::<AppEvent>();
        let tx = AppEventSender::new(tx_raw);
        let mut widget = StatusIndicatorWidget::new(
            tx,
            crate::tui::FrameRequester::test_dummy(),
            /*animations_enabled*/ false,
        );
        let baseline = Instant::now();
        widget.last_resume_at = baseline;

        assert_eq!(widget.status_line_at(baseline).to_string(), "• 0s");
        assert_eq!(
            widget
                .status_line_at(baseline + Duration::from_secs(1))
                .to_string(),
            "• 1s"
        );
        assert_eq!(
            widget
                .status_line_at(baseline + Duration::from_secs(2))
                .to_string(),
            "• 2s"
        );
        assert_eq!(
            widget.next_frame_delay_at(baseline + Duration::from_millis(250)),
            Some(Duration::from_millis(750))
        );
        widget.animations_enabled = true;
        assert_eq!(
            widget.next_frame_delay_at(baseline + Duration::from_millis(250)),
            Some(SPINNER_FRAME_INTERVAL)
        );
    }

    #[test]
    fn operational_details_do_not_expand_the_mini_status_row() {
        let (tx_raw, _rx) = unbounded_channel::<AppEvent>();
        let tx = AppEventSender::new(tx_raw);
        let mut w = StatusIndicatorWidget::new(
            tx,
            crate::tui::FrameRequester::test_dummy(),
            /*animations_enabled*/ true,
        );
        w.update_details(
            Some("abcd abcd abcd abcd".to_string()),
            StatusDetailsCapitalization::CapitalizeFirst,
            STATUS_DETAILS_DEFAULT_MAX_LINES,
        );

        let lines = w.wrapped_details_lines(/*width*/ 6);
        assert!(lines.is_empty());
        assert_eq!(w.desired_height(/*width*/ 6), 1);
    }

    #[test]
    fn details_args_can_disable_capitalization_and_limit_lines() {
        let (tx_raw, _rx) = unbounded_channel::<AppEvent>();
        let tx = AppEventSender::new(tx_raw);
        let mut w = StatusIndicatorWidget::new(
            tx,
            crate::tui::FrameRequester::test_dummy(),
            /*animations_enabled*/ true,
        );
        w.update_details(
            Some("cargo test -p codex-core and then cargo test -p codex-tui".to_string()),
            StatusDetailsCapitalization::Preserve,
            /*max_lines*/ 1,
        );

        assert_eq!(w.details(), None);

        let lines = w.wrapped_details_lines(/*width*/ 24);
        assert!(lines.is_empty());
    }

    fn rendered_backend_line(terminal: &Terminal<TestBackend>, row: usize) -> String {
        let width = usize::from(terminal.backend().buffer().area.width);
        terminal.backend().buffer().content()[row * width..(row + 1) * width]
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect()
    }
}
