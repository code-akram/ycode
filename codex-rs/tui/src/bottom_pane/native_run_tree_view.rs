use std::time::Duration;
use std::time::Instant;

#[cfg(test)]
use codex_cli_runtime_client::native_run_tree::NATIVE_RUN_TREE_MAX_REFS;
#[cfg(test)]
use codex_cli_runtime_client::native_run_tree::NATIVE_RUN_TREE_RECENT_BYTES;
#[cfg(test)]
use codex_cli_runtime_client::native_run_tree::NATIVE_RUN_TREE_REF_BYTES;
#[cfg(test)]
use codex_cli_runtime_client::native_run_tree::NATIVE_RUN_TREE_SUMMARY_BYTES;
use codex_cli_runtime_client::native_run_tree::NativeRunCancelScope;
use codex_cli_runtime_client::native_run_tree::NativeRunNode;
use codex_cli_runtime_client::native_run_tree::NativeRunNodeKind;
use codex_cli_runtime_client::native_run_tree::NativeRunNodeStatus;
use codex_cli_runtime_client::native_run_tree::NativeRunTreeSnapshot;
use crossterm::event::KeyCode;
use crossterm::event::KeyEvent;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::style::Stylize;
use ratatui::text::Line;
use ratatui::text::Span;
use ratatui::widgets::Clear;
use ratatui::widgets::Paragraph;
use ratatui::widgets::Widget;
use tokio::sync::watch;
use unicode_width::UnicodeWidthChar;
use unicode_width::UnicodeWidthStr;

use crate::app_event::AppEvent;
use crate::app_event_sender::AppEventSender;
use crate::render::renderable::Renderable;

use super::BottomPaneView;
use super::CancellationEvent;
use super::ViewCompletion;

const FRAME_DELAY: Duration = Duration::from_millis(100);
const MAX_VISIBLE_NODES: usize = 12;

pub(crate) struct NativeRunTreeView {
    receiver: watch::Receiver<Option<NativeRunTreeSnapshot>>,
    snapshot: NativeRunTreeSnapshot,
    selected: usize,
    detail: bool,
    detail_scroll: usize,
    completion: Option<ViewCompletion>,
    app_event_tx: AppEventSender,
}

impl NativeRunTreeView {
    pub(crate) fn new(
        receiver: watch::Receiver<Option<NativeRunTreeSnapshot>>,
        app_event_tx: AppEventSender,
    ) -> Result<Self, String> {
        let snapshot = receiver
            .borrow()
            .clone()
            .ok_or_else(|| "native run already settled".to_string())?;
        Ok(Self {
            receiver,
            snapshot,
            selected: 0,
            detail: false,
            detail_scroll: 0,
            completion: None,
            app_event_tx,
        })
    }

    fn selected_node(&self) -> Option<&NativeRunNode> {
        self.snapshot.nodes.get(self.selected)
    }

    fn cancel_selected(&self) {
        let Some(node) = self.selected_node() else {
            return;
        };
        if node.cancel_scope == NativeRunCancelScope::None
            || node.status != NativeRunNodeStatus::Running
        {
            return;
        }
        self.app_event_tx.send(AppEvent::CancelNativeCodeModeNode {
            thread_id: codex_protocol::ThreadId::from_string(&self.snapshot.identity.thread_id)
                .unwrap_or_else(|_| unreachable!("validated native thread id")),
            run_id: self.snapshot.identity.run_id.clone(),
            node_id: node.stable_id.clone(),
        });
    }

    fn visible_range(&self) -> std::ops::Range<usize> {
        let start = self
            .selected
            .saturating_sub(MAX_VISIBLE_NODES.saturating_sub(1));
        start..self.snapshot.nodes.len().min(start + MAX_VISIBLE_NODES)
    }
}

impl BottomPaneView for NativeRunTreeView {
    fn handle_key_event(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Up | KeyCode::Char('k') if self.detail => {
                self.detail_scroll = self.detail_scroll.saturating_sub(1)
            }
            KeyCode::Down | KeyCode::Char('j') if self.detail => {
                self.detail_scroll = self.detail_scroll.saturating_add(1)
            }
            KeyCode::Up | KeyCode::Char('k') => self.selected = self.selected.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => {
                self.selected = (self.selected + 1).min(self.snapshot.nodes.len().saturating_sub(1))
            }
            KeyCode::Enter | KeyCode::Right => {
                self.detail = true;
                self.detail_scroll = 0;
            }
            KeyCode::Left if self.detail => {
                self.detail = false;
                self.detail_scroll = 0;
            }
            KeyCode::Esc | KeyCode::Left => self.completion = Some(ViewCompletion::Cancelled),
            KeyCode::Char('x') => self.cancel_selected(),
            _ => {}
        }
    }

    fn prefer_esc_to_handle_key_event(&self) -> bool {
        true
    }

    fn on_ctrl_c(&mut self) -> CancellationEvent {
        self.completion = Some(ViewCompletion::Cancelled);
        CancellationEvent::Handled
    }

    fn is_complete(&self) -> bool {
        self.completion.is_some()
    }
    fn completion(&self) -> Option<ViewCompletion> {
        self.completion
    }

    fn pre_draw_tick(&mut self, _now: Instant) -> bool {
        if !self.receiver.has_changed().unwrap_or(true) {
            return false;
        }
        let next = self.receiver.borrow_and_update().clone();
        match next {
            Some(snapshot) => {
                let selected_id = self.selected_node().map(|node| node.stable_id.clone());
                self.snapshot = snapshot;
                self.selected = selected_id
                    .as_ref()
                    .and_then(|selected_id| {
                        self.snapshot
                            .nodes
                            .iter()
                            .position(|node| &node.stable_id == selected_id)
                    })
                    .unwrap_or_else(|| {
                        self.selected
                            .min(self.snapshot.nodes.len().saturating_sub(1))
                    });
            }
            None => self.completion = Some(ViewCompletion::Accepted),
        }
        true
    }

    fn next_frame_delay(&self) -> Option<Duration> {
        Some(FRAME_DELAY)
    }
}

impl Renderable for NativeRunTreeView {
    fn desired_height(&self, _width: u16) -> u16 {
        if self.detail {
            12
        } else {
            (self.snapshot.nodes.len().min(MAX_VISIBLE_NODES) as u16).saturating_add(3)
        }
    }

    fn render(&self, area: Rect, buf: &mut Buffer) {
        Clear.render(area, buf);
        if area.is_empty() {
            return;
        }
        Paragraph::new(Line::from(Span::styled(
            "Code mode run",
            crate::style::operational_accent_style(),
        )))
        .render(Rect::new(area.x, area.y, area.width, 1), buf);
        let body = Rect::new(
            area.x,
            area.y.saturating_add(1),
            area.width,
            area.height.saturating_sub(2),
        );
        if self.detail {
            if let Some(node) = self.selected_node() {
                let details = detail_lines(node, body.width);
                let available = usize::from(body.height);
                let max_scroll = details.len().saturating_sub(available);
                let scroll = self.detail_scroll.min(max_scroll);
                Paragraph::new(
                    details
                        .into_iter()
                        .skip(scroll)
                        .take(available)
                        .collect::<Vec<_>>(),
                )
                .render(body, buf);
            }
        } else {
            let mut lines = Vec::new();
            for index in self.visible_range() {
                let node = &self.snapshot.nodes[index];
                let indent = "  ".repeat(node_depth(&self.snapshot, index));
                let marker = if index == self.selected { "›" } else { " " };
                let elapsed = node
                    .finished_at
                    .unwrap_or_else(Instant::now)
                    .duration_since(node.started_at);
                let text = format!(
                    "{marker} {indent}{} · {} · {}",
                    kind_label(node.kind),
                    status_label(node.status),
                    elapsed_label(elapsed)
                );
                let style = if index == self.selected {
                    crate::style::operational_reference_style().reversed()
                } else {
                    ratatui::style::Style::default()
                };
                lines.push(Line::from(Span::styled(text, style)));
            }
            if let Some(error) = &self.snapshot.local_error {
                lines.push(Line::from(Span::styled(
                    error.clone(),
                    ratatui::style::Style::default().red(),
                )));
            }
            Paragraph::new(lines).render(body, buf);
        }
        let cancel_help = self
            .selected_node()
            .filter(|node| node.status == NativeRunNodeStatus::Running)
            .map(|node| match node.cancel_scope {
                NativeRunCancelScope::Run => " · x cancel run",
                NativeRunCancelScope::Call => " · x cancel call",
                NativeRunCancelScope::Agent => " · x cancel agent",
                NativeRunCancelScope::None => "",
            })
            .unwrap_or_default();
        if area.height >= 2 {
            Paragraph::new(Line::from(
                format!("↑/↓ j/k navigate · Enter/right details · Esc/left back{cancel_help}")
                    .dim(),
            ))
            .render(
                Rect::new(area.x, area.bottom().saturating_sub(1), area.width, 1),
                buf,
            );
        }
    }
}

fn detail_lines(node: &NativeRunNode, width: u16) -> Vec<Line<'static>> {
    let elapsed = node
        .finished_at
        .unwrap_or_else(Instant::now)
        .duration_since(node.started_at);
    let mut lines = Vec::new();
    push_wrapped_detail(
        &mut lines,
        "kind · ",
        &kind_label(node.kind),
        Style::default(),
        width,
    );
    push_wrapped_detail(
        &mut lines,
        "id · ",
        &node.stable_id,
        Style::default(),
        width,
    );
    push_wrapped_detail(
        &mut lines,
        "status · ",
        status_label(node.status),
        Style::default(),
        width,
    );
    push_wrapped_detail(
        &mut lines,
        "elapsed · ",
        &elapsed_label(elapsed),
        Style::default(),
        width,
    );
    push_wrapped_detail(
        &mut lines,
        "summary · ",
        &node.summary,
        Style::default(),
        width,
    );
    match node.cancel_scope {
        NativeRunCancelScope::Run => push_wrapped_detail(
            &mut lines,
            "cancel · ",
            "terminates run",
            Style::default(),
            width,
        ),
        NativeRunCancelScope::Call => push_wrapped_detail(
            &mut lines,
            "cancel · ",
            "selected call",
            Style::default(),
            width,
        ),
        NativeRunCancelScope::Agent => push_wrapped_detail(
            &mut lines,
            "cancel · ",
            "selected agent",
            Style::default(),
            width,
        ),
        NativeRunCancelScope::None => {}
    }
    if !node.recent.is_empty() {
        push_wrapped_detail(
            &mut lines,
            "recent · ",
            &node.recent,
            Style::default(),
            width,
        );
    }
    for reference in &node.artifact_refs {
        push_wrapped_detail(
            &mut lines,
            "ref · ",
            reference,
            crate::style::operational_reference_style(),
            width,
        );
    }
    lines
}

fn push_wrapped_detail(
    lines: &mut Vec<Line<'static>>,
    label: &str,
    value: &str,
    value_style: Style,
    width: u16,
) {
    let width = usize::from(width.max(1));
    let label_width = UnicodeWidthStr::width(label);
    if label_width >= width {
        for chunk in wrap_display(&format!("{label}{value}"), width) {
            lines.push(Line::from(Span::styled(chunk, value_style)));
        }
        return;
    }
    let chunks = wrap_display(value, width - label_width);
    for (index, chunk) in chunks.into_iter().enumerate() {
        let prefix = if index == 0 {
            label.to_string()
        } else {
            " ".repeat(label_width)
        };
        lines.push(Line::from(vec![
            Span::raw(prefix),
            Span::styled(chunk, value_style),
        ]));
    }
}

fn wrap_display(value: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut rows = Vec::new();
    let mut row = String::new();
    let mut row_width: usize = 0;
    for character in value.chars() {
        let character_width = UnicodeWidthChar::width(character).unwrap_or(0);
        if !row.is_empty() && row_width.saturating_add(character_width) > width {
            rows.push(std::mem::take(&mut row));
            row_width = 0;
        }
        row.push(character);
        row_width = row_width.saturating_add(character_width);
    }
    if !row.is_empty() || rows.is_empty() {
        rows.push(row);
    }
    rows
}

fn node_depth(snapshot: &NativeRunTreeSnapshot, index: usize) -> usize {
    let mut depth = 0;
    let mut parent = snapshot.nodes[index].parent_id.as_deref();
    while let Some(parent_id) = parent {
        let Some(parent_node) = snapshot
            .nodes
            .iter()
            .find(|node| node.stable_id == parent_id)
        else {
            break;
        };
        depth += 1;
        if depth >= snapshot.nodes.len() {
            break;
        }
        parent = parent_node.parent_id.as_deref();
    }
    depth
}

fn kind_label(kind: NativeRunNodeKind) -> String {
    match kind {
        NativeRunNodeKind::Run => "run".to_string(),
        NativeRunNodeKind::Generation => "generation".to_string(),
        NativeRunNodeKind::Compile {
            attempt,
            pid: Some(pid),
        } => format!("compile {attempt} · pid {pid}"),
        NativeRunNodeKind::Compile { attempt, pid: None } => format!("compile {attempt}"),
        NativeRunNodeKind::Repair => "repair".to_string(),
        NativeRunNodeKind::Workflow {
            attempt,
            pid: Some(pid),
        } => format!("workflow {attempt} · pid {pid}"),
        NativeRunNodeKind::Workflow { attempt, pid: None } => format!("workflow {attempt}"),
        NativeRunNodeKind::ToolCall => "tool call".to_string(),
        NativeRunNodeKind::Agent => "agent".to_string(),
        NativeRunNodeKind::Process { pid } => format!("process · pid {pid}"),
        NativeRunNodeKind::Finalization => "finalization".to_string(),
    }
}

fn status_label(status: NativeRunNodeStatus) -> &'static str {
    match status {
        NativeRunNodeStatus::Running => "running",
        NativeRunNodeStatus::Cancelling => "cancelling",
        NativeRunNodeStatus::Succeeded => "succeeded",
        NativeRunNodeStatus::Failed => "failed",
        NativeRunNodeStatus::Cancelled => "cancelled",
    }
}

fn elapsed_label(duration: Duration) -> String {
    if duration.as_secs() >= 10 {
        format!("{}s", duration.as_secs())
    } else {
        format!("{:.1}s", duration.as_secs_f32())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use codex_cli_runtime_client::native_run_tree::NativeRunIdentity;
    use codex_cli_runtime_client::native_run_tree::NativeRunNode;
    use crossterm::execute;
    use crossterm::terminal::EnterAlternateScreen;
    use crossterm::terminal::LeaveAlternateScreen;
    use ratatui::Terminal;
    use ratatui::backend::CrosstermBackend;
    use ratatui::style::Modifier;
    use std::fs::File;
    use std::io::Read as _;
    use std::io::Write as _;
    use std::os::fd::AsRawFd;
    use std::os::fd::FromRawFd;
    use std::os::fd::OwnedFd;
    use std::process::Command;
    use std::process::Stdio;

    fn snapshot() -> NativeRunTreeSnapshot {
        let started_at = Instant::now();
        NativeRunTreeSnapshot {
            identity: NativeRunIdentity {
                session_id: "019ffc6d-c0b5-7a01-9f17-3ab4571834ce".into(),
                thread_id: "019ffc6d-c0b5-7a01-9f17-3ab4571834ce".into(),
                run_id: "2154af33-1fed-4fb4-a821-156a07336f20".into(),
            },
            nodes: vec![
                NativeRunNode {
                    stable_id: "run".into(),
                    parent_id: None,
                    launch_ordinal: 0,
                    kind: NativeRunNodeKind::Run,
                    status: NativeRunNodeStatus::Running,
                    started_at,
                    finished_at: None,
                    summary: "inspect".into(),
                    recent: String::new(),
                    artifact_refs: Vec::new(),
                    cancel_scope: NativeRunCancelScope::Run,
                },
                NativeRunNode {
                    stable_id: "generation".into(),
                    parent_id: Some("run".into()),
                    launch_ordinal: 1,
                    kind: NativeRunNodeKind::Generation,
                    status: NativeRunNodeStatus::Running,
                    started_at,
                    finished_at: None,
                    summary: "source generation".into(),
                    recent: String::new(),
                    artifact_refs: Vec::new(),
                    cancel_scope: NativeRunCancelScope::None,
                },
            ],
            local_error: None,
        }
    }

    fn rendered(view: &NativeRunTreeView, width: u16) -> Buffer {
        let area = Rect::new(0, 0, width, view.desired_height(width));
        let mut buffer = Buffer::empty(area);
        view.render(area, &mut buffer);
        buffer
    }

    fn rendered_lines(view: &NativeRunTreeView, width: u16, height: u16) -> Vec<String> {
        let area = Rect::new(0, 0, width, height);
        let mut buffer = Buffer::empty(area);
        view.render(area, &mut buffer);
        (0..height)
            .map(|row| {
                (0..width)
                    .map(|column| buffer[(column, row)].symbol())
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect()
    }

    #[test]
    fn renders_regular_weight_navigation_and_detail_at_narrow_width() {
        let (_tx, rx) = watch::channel(Some(snapshot()));
        let (events, _event_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut view = NativeRunTreeView::new(rx, AppEventSender::new(events)).expect("view");
        let buffer = rendered(&view, 36);
        let text = buffer
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(text.contains("Code mode run"));
        assert!(text.contains("generation"));
        assert!(
            buffer
                .content
                .iter()
                .all(|cell| !cell.modifier.contains(Modifier::BOLD))
        );
        view.handle_key_event(KeyEvent::from(KeyCode::Down));
        view.handle_key_event(KeyEvent::from(KeyCode::Enter));
        let detail = rendered(&view, 30)
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(detail.contains("id · generation"));
        view.handle_key_event(KeyEvent::from(KeyCode::Left));
        assert!(!view.detail);
    }

    #[test]
    fn agent_node_has_truthful_selective_cancel_label_and_bounded_detail() {
        let mut snapshot = snapshot();
        snapshot.nodes.push(NativeRunNode {
            stable_id: "agent-native-run-a1-1".into(),
            parent_id: Some("run".into()),
            launch_ordinal: 2,
            kind: NativeRunNodeKind::Agent,
            status: NativeRunNodeStatus::Running,
            started_at: Instant::now(),
            finished_at: None,
            summary: "agent task · 19 bytes · model inherited · reasoning inherited".into(),
            recent: "pending".into(),
            artifact_refs: Vec::new(),
            cancel_scope: NativeRunCancelScope::Agent,
        });
        let (_tx, rx) = watch::channel(Some(snapshot));
        let (events, _event_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut view = NativeRunTreeView::new(rx, AppEventSender::new(events)).expect("view");
        view.handle_key_event(KeyEvent::from(KeyCode::Down));
        view.handle_key_event(KeyEvent::from(KeyCode::Down));
        let tree = rendered_lines(&view, 72, 8).join("\n");
        assert!(tree.contains("agent"));
        assert!(tree.contains("x cancel agent"));
        view.handle_key_event(KeyEvent::from(KeyCode::Enter));
        let details = rendered_lines(&view, 48, 10).join("\n");
        assert!(details.contains("kind · agent"));
        assert!(details.contains("cancel · selected agent"));
    }

    #[test]
    fn renders_true_ancestry_and_scrolls_to_bounded_detail_references() {
        let mut snapshot = snapshot();
        let started_at = Instant::now();
        snapshot.nodes.extend([
            NativeRunNode {
                stable_id: "workflow-a1".into(),
                parent_id: Some("run".into()),
                launch_ordinal: 2,
                kind: NativeRunNodeKind::Workflow {
                    attempt: 1,
                    pid: Some(42),
                },
                status: NativeRunNodeStatus::Running,
                started_at,
                finished_at: None,
                summary: "workflow".into(),
                recent: String::new(),
                artifact_refs: Vec::new(),
                cancel_scope: NativeRunCancelScope::Run,
            },
            NativeRunNode {
                stable_id: "call-1".into(),
                parent_id: Some("workflow-a1".into()),
                launch_ordinal: 3,
                kind: NativeRunNodeKind::ToolCall,
                status: NativeRunNodeStatus::Running,
                started_at,
                finished_at: None,
                summary: "shell request".into(),
                recent: "bounded result".into(),
                artifact_refs: (0..NATIVE_RUN_TREE_MAX_REFS)
                    .map(|index| format!("native-code-mode://thread/run/ref-{index}"))
                    .collect(),
                cancel_scope: NativeRunCancelScope::Call,
            },
        ]);
        let (_tx, rx) = watch::channel(Some(snapshot));
        let (events, _event_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut view = NativeRunTreeView::new(rx, AppEventSender::new(events)).expect("view");
        let lines = rendered_lines(&view, 80, 8);
        let workflow = lines
            .iter()
            .find(|line| line.contains("workflow 1"))
            .expect("workflow row");
        let call = lines
            .iter()
            .find(|line| line.contains("tool call"))
            .expect("call row");
        assert!(call.find("tool call").unwrap() > workflow.find("workflow 1").unwrap());

        for _ in 0..3 {
            view.handle_key_event(KeyEvent::from(KeyCode::Down));
        }
        view.handle_key_event(KeyEvent::from(KeyCode::Enter));
        for _ in 0..NATIVE_RUN_TREE_MAX_REFS + 8 {
            view.handle_key_event(KeyEvent::from(KeyCode::Down));
        }
        let details = rendered_lines(&view, 64, 8).join("\n");
        assert!(details.contains("ref-15"));
        assert!(!details.contains("ref-0"));
    }

    #[test]
    fn width_48_wraps_full_detail_caps_and_scroll_reaches_multibyte_and_ref_tails() {
        let mut snapshot = snapshot();
        let summary_tail = "SUMMARY-END";
        let recent_tail = "RECENT-END";
        let ref_tail = "REF-END";
        let summary = format!(
            "{}{}",
            "s".repeat(NATIVE_RUN_TREE_SUMMARY_BYTES - summary_tail.len()),
            summary_tail
        );
        let recent_prefix = "界".repeat((NATIVE_RUN_TREE_RECENT_BYTES - recent_tail.len()) / 3);
        let recent = format!("{recent_prefix}{recent_tail}");
        let ref_prefix = "native-code-mode://thread/run/";
        let reference = format!(
            "{ref_prefix}{}{}",
            "r".repeat(NATIVE_RUN_TREE_REF_BYTES - ref_prefix.len() - ref_tail.len()),
            ref_tail
        );
        assert_eq!(summary.len(), NATIVE_RUN_TREE_SUMMARY_BYTES);
        assert!(recent.len() <= NATIVE_RUN_TREE_RECENT_BYTES);
        assert_eq!(reference.len(), NATIVE_RUN_TREE_REF_BYTES);
        snapshot.nodes[1].summary = summary;
        snapshot.nodes[1].recent = recent;
        snapshot.nodes[1].artifact_refs = vec![reference];

        let (_tx, rx) = watch::channel(Some(snapshot));
        let (events, _event_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut view = NativeRunTreeView::new(rx, AppEventSender::new(events)).expect("view");
        view.handle_key_event(KeyEvent::from(KeyCode::Down));
        view.handle_key_event(KeyEvent::from(KeyCode::Enter));

        let mut rendered_during_scroll = String::new();
        for _ in 0..80 {
            rendered_during_scroll.push_str(&rendered_lines(&view, 48, 12).join("\n"));
            view.handle_key_event(KeyEvent::from(KeyCode::Down));
        }
        let rendered_without_row_breaks = rendered_during_scroll
            .chars()
            .filter(|character| !character.is_whitespace())
            .collect::<String>();
        assert!(rendered_without_row_breaks.contains(summary_tail));
        assert!(rendered_without_row_breaks.contains(recent_tail));
        assert!(rendered_without_row_breaks.contains(ref_tail));
    }

    #[test]
    fn active_overlay_offline_pty_proves_navigation_reflow_control_and_restoration() {
        if std::env::var_os("YCODE_NATIVE_TREE_PTY_CHILD").is_some() {
            return;
        }
        let mut master_fd = -1;
        let mut slave_fd = -1;
        let mut size = libc::winsize {
            ws_row: 18,
            ws_col: 72,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        // SAFETY: openpty initializes both descriptors and reads the supplied window size.
        assert_eq!(
            unsafe {
                libc::openpty(
                    &mut master_fd,
                    &mut slave_fd,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    &raw mut size,
                )
            },
            0
        );
        // SAFETY: successful openpty returns two uniquely owned descriptors.
        let mut master = File::from(unsafe { OwnedFd::from_raw_fd(master_fd) });
        // SAFETY: successful openpty returns a second uniquely owned descriptor.
        let slave = File::from(unsafe { OwnedFd::from_raw_fd(slave_fd) });
        let initial_flags = terminal_flags(&slave);
        let mut child = Command::new(std::env::current_exe().expect("current test executable"))
            .args([
                "--exact",
                "bottom_pane::native_run_tree_view::tests::native_run_tree_pty_child",
                "--nocapture",
            ])
            .env("YCODE_NATIVE_TREE_PTY_CHILD", "1")
            .env("TERM", "xterm-256color")
            .stdin(Stdio::from(slave.try_clone().expect("clone pty stdin")))
            .stdout(Stdio::from(slave.try_clone().expect("clone pty stdout")))
            .stderr(Stdio::from(slave.try_clone().expect("clone pty stderr")))
            .spawn()
            .expect("spawn offline tree pty child");
        set_nonblocking(&master);
        let mut parser = vt100::Parser::new(18, 72, 0);
        let mut output = Vec::new();
        wait_for_pty_text(
            &mut master,
            &mut parser,
            &mut output,
            &mut child,
            "Code mode run",
        );
        let initial = parser.screen().contents();
        let workflow = initial
            .lines()
            .find(|line| line.contains("workflow 1"))
            .expect("workflow row");
        let call = initial
            .lines()
            .find(|line| line.contains("tool call"))
            .expect("tool row");
        assert!(call.find("tool call").unwrap() > workflow.find("workflow 1").unwrap());
        std::thread::sleep(Duration::from_millis(260));
        for _ in 0..16 {
            read_pty(
                &mut master,
                &mut parser,
                &mut output,
                Duration::from_millis(10),
            );
        }
        assert!(
            ["0.2s", "0.3s", "0.4s"]
                .iter()
                .any(|elapsed| parser.screen().contents().contains(elapsed))
        );

        size.ws_row = 14;
        size.ws_col = 48;
        // SAFETY: master is a live pty and size remains valid for the ioctl.
        assert_ne!(
            unsafe { libc::ioctl(master.as_raw_fd(), libc::TIOCSWINSZ, &size) },
            -1
        );
        // SAFETY: child is live and SIGWINCH requests ordinary terminal reflow.
        assert_ne!(
            unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGWINCH) },
            -1
        );
        parser.screen_mut().set_size(14, 48);
        master.write_all(b"jjj\r").expect("open call detail");
        master.flush().expect("flush detail input");
        wait_for_pty_text(
            &mut master,
            &mut parser,
            &mut output,
            &mut child,
            "id · call-1",
        );
        master
            .write_all(b"jjjjjjjjjjjjjjjjjjjj")
            .expect("scroll detail");
        master.flush().expect("flush scroll input");
        wait_for_pty_text(&mut master, &mut parser, &mut output, &mut child, "ref-15");
        master.write_all(b"x").expect("cancel selected call");
        master.flush().expect("flush cancel input");
        std::thread::sleep(Duration::from_millis(100));
        master.write_all(b"\x1b").expect("dismiss overlay");
        master.flush().expect("flush escape input");
        wait_for_pty_text(
            &mut master,
            &mut parser,
            &mut output,
            &mut child,
            "draft survives overlay",
        );

        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline && child.try_wait().expect("query pty child").is_none() {
            read_pty(
                &mut master,
                &mut parser,
                &mut output,
                Duration::from_millis(20),
            );
        }
        read_pty(
            &mut master,
            &mut parser,
            &mut output,
            Duration::from_millis(50),
        );
        assert!(child.try_wait().expect("query settled pty child").is_some());
        assert!(String::from_utf8_lossy(&output).contains("NATIVE_TREE_PTY_CANCEL=call-1"));
        assert!(!output.windows(4).any(|window| window == b"\x1b[1m"));
        assert!(String::from_utf8_lossy(&output).contains("0.0s"));
        let restored_flags = terminal_flags(&slave);
        let restored = libc::ICANON | libc::ECHO;
        assert_eq!(restored_flags & restored, initial_flags & restored);
    }

    #[test]
    fn native_run_tree_pty_child() {
        if std::env::var_os("YCODE_NATIVE_TREE_PTY_CHILD").is_none() {
            return;
        }
        crossterm::terminal::enable_raw_mode().expect("enable child raw mode");
        let mut stdout = std::io::stdout();
        execute!(stdout, EnterAlternateScreen).expect("enter alternate screen");
        let (snapshot_tx, snapshot_rx) = watch::channel(Some(pty_snapshot()));
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime");
        let (mut chat, _event_tx, mut event_rx, _op_rx) =
            runtime.block_on(crate::chatwidget::tests::make_chatwidget_manual_with_sender());
        chat.set_composer_text_for_test("draft survives overlay");
        chat.handle_server_notification(
            codex_cli_protocol::ServerNotification::TurnStarted(
                codex_cli_protocol::TurnStartedNotification {
                    thread_id: String::new(),
                    turn: codex_cli_protocol::Turn {
                        id: "native-tree-turn".to_string(),
                        items_view: codex_cli_protocol::TurnItemsView::Full,
                        items: Vec::new(),
                        status: codex_cli_protocol::TurnStatus::InProgress,
                        error: None,
                        started_at: None,
                        completed_at: None,
                        duration_ms: None,
                    },
                },
            ),
            /*replay_kind*/ None,
        );
        chat.show_native_run_tree(snapshot_rx);
        let mut terminal = Terminal::new(CrosstermBackend::new(stdout)).expect("terminal");
        let started = Instant::now();
        let mut cancelled = None;
        while chat.has_active_view() && started.elapsed() < Duration::from_secs(5) {
            terminal.autoresize().expect("resize child terminal");
            terminal
                .draw(|frame| chat.render(frame.area(), frame.buffer_mut()))
                .expect("render child tree");
            if let Ok(AppEvent::CancelNativeCodeModeNode { node_id, .. }) = event_rx.try_recv() {
                cancelled = Some(node_id);
            }
            if crossterm::event::poll(Duration::from_millis(50)).expect("poll child input")
                && let crossterm::event::Event::Key(key) =
                    crossterm::event::read().expect("read child key")
            {
                chat.handle_key_event(key);
            }
        }
        assert!(!chat.has_active_view(), "tree overlay dismissed");
        terminal.autoresize().expect("resize restored composer");
        terminal
            .draw(|frame| chat.render(frame.area(), frame.buffer_mut()))
            .expect("render restored composer");
        let restored_area = terminal.size().expect("terminal size");
        assert!(chat.cursor_pos(restored_area.into()).is_some());
        assert_eq!(chat.composer_text_for_test(), "draft survives overlay");
        std::thread::sleep(Duration::from_millis(250));
        drop(snapshot_tx);
        execute!(terminal.backend_mut(), LeaveAlternateScreen).expect("leave alternate screen");
        crossterm::terminal::disable_raw_mode().expect("disable child raw mode");
        println!(
            "NATIVE_TREE_PTY_CANCEL={} RESTORED=draft,cursor,status",
            cancelled.unwrap_or_default()
        );
    }

    #[test]
    fn updates_event_driven_and_closes_on_terminal_settlement() {
        let (tx, rx) = watch::channel(Some(snapshot()));
        let (events, _event_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut view = NativeRunTreeView::new(rx, AppEventSender::new(events)).expect("view");
        assert_eq!(view.next_frame_delay(), Some(Duration::from_millis(100)));
        tx.send(None).expect("settle");
        assert!(view.pre_draw_tick(Instant::now()));
        assert!(view.is_complete());
    }

    #[tokio::test]
    async fn x_routes_exact_selected_owner_and_help_has_no_pause_or_restart() {
        let (_tx, rx) = watch::channel(Some(snapshot()));
        let (events, mut event_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut view = NativeRunTreeView::new(rx, AppEventSender::new(events)).expect("view");
        view.handle_key_event(KeyEvent::from(KeyCode::Char('x')));
        let event = event_rx.recv().await.expect("cancel event");
        assert!(
            matches!(event, AppEvent::CancelNativeCodeModeNode { node_id, .. } if node_id == "run")
        );
        let text = rendered(&view, 90)
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(text.contains("x cancel run"));
        assert!(!text.contains("pause"));
        assert!(!text.contains("restart"));
        view.handle_key_event(KeyEvent::from(KeyCode::Down));
        let non_cancellable = rendered(&view, 90)
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(!non_cancellable.contains("x cancel"));
    }

    fn pty_snapshot() -> NativeRunTreeSnapshot {
        let mut snapshot = snapshot();
        let started_at = Instant::now();
        snapshot.nodes.extend([
            NativeRunNode {
                stable_id: "workflow-a1".into(),
                parent_id: Some("run".into()),
                launch_ordinal: 2,
                kind: NativeRunNodeKind::Workflow {
                    attempt: 1,
                    pid: Some(42),
                },
                status: NativeRunNodeStatus::Running,
                started_at,
                finished_at: None,
                summary: "workflow".into(),
                recent: String::new(),
                artifact_refs: Vec::new(),
                cancel_scope: NativeRunCancelScope::Run,
            },
            NativeRunNode {
                stable_id: "call-1".into(),
                parent_id: Some("workflow-a1".into()),
                launch_ordinal: 3,
                kind: NativeRunNodeKind::ToolCall,
                status: NativeRunNodeStatus::Running,
                started_at,
                finished_at: None,
                summary: "shell request".into(),
                recent: "bounded result".into(),
                artifact_refs: (0..NATIVE_RUN_TREE_MAX_REFS)
                    .map(|index| format!("native-code-mode://thread/run/ref-{index}"))
                    .collect(),
                cancel_scope: NativeRunCancelScope::Call,
            },
        ]);
        snapshot
    }

    fn terminal_flags(terminal: &File) -> libc::tcflag_t {
        let mut attributes = std::mem::MaybeUninit::<libc::termios>::uninit();
        // SAFETY: attributes is writable and terminal owns a live pty descriptor.
        assert_ne!(
            unsafe { libc::tcgetattr(terminal.as_raw_fd(), attributes.as_mut_ptr()) },
            -1
        );
        // SAFETY: successful tcgetattr initialized the termios structure.
        unsafe { attributes.assume_init() }.c_lflag
    }

    fn set_nonblocking(file: &File) {
        // SAFETY: file owns a live descriptor and F_GETFL does not mutate memory.
        let flags = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETFL) };
        assert_ne!(flags, -1);
        // SAFETY: descriptor and flags are valid for F_SETFL.
        assert_ne!(
            unsafe { libc::fcntl(file.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) },
            -1
        );
    }

    fn read_pty(
        master: &mut File,
        parser: &mut vt100::Parser,
        output: &mut Vec<u8>,
        timeout: Duration,
    ) {
        let mut descriptor = libc::pollfd {
            fd: master.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: descriptor points to one initialized pollfd for the duration of the call.
        let ready = unsafe {
            libc::poll(
                &mut descriptor,
                1,
                timeout.as_millis().min(libc::c_int::MAX as u128) as libc::c_int,
            )
        };
        if ready <= 0 || descriptor.revents & libc::POLLIN == 0 {
            return;
        }
        let mut bytes = [0_u8; 8192];
        match master.read(&mut bytes) {
            Ok(count) => {
                output.extend_from_slice(&bytes[..count]);
                parser.process(&bytes[..count]);
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(error) => panic!("read offline pty: {error}"),
        }
    }

    fn wait_for_pty_text(
        master: &mut File,
        parser: &mut vt100::Parser,
        output: &mut Vec<u8>,
        child: &mut std::process::Child,
        expected: &str,
    ) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            read_pty(master, parser, output, Duration::from_millis(20));
            if parser.screen().contents().contains(expected) {
                return;
            }
            if let Some(status) = child.try_wait().expect("query pty child") {
                panic!(
                    "offline pty child exited {status} before {expected:?}:\n{}",
                    parser.screen().contents()
                );
            }
        }
        panic!(
            "offline pty never rendered {expected:?}:\n{}",
            parser.screen().contents()
        );
    }
}
