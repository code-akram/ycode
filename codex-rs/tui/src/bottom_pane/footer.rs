//! The bottom-pane footer renders transient hints and context indicators.
//!
//! The footer is pure rendering: it formats `FooterProps` into `Line`s without mutating any state.
//! It intentionally does not decide *which* footer content should be shown; that is owned by the
//! `ChatComposer` (which selects a `FooterMode`) and by higher-level state machines like
//! `ChatWidget` (which decides when quit/interrupt is allowed).
//!
//! Some footer content is time-based rather than event-based, such as the "press again to quit"
//! hint. The owning widgets schedule redraws so time-based hints can expire even if the UI is
//! otherwise idle.
//!
//! Terminology used in this module:
//! - "status line" means the configurable contextual row built from `/statusline` items such as
//!   model, git branch, and context usage.
//! - "instructional footer" means a row that tells the user what to do next, such as quit
//!   confirmation, shortcut help, or queue hints.
//! - "contextual footer" means the footer is free to show ambient context instead of an
//!   instruction. In that state, the footer may render the configured status line, the active
//!   agent label, side-conversation state, or some combination of those.
//!
//! Single-line collapse overview:
//! 1. The composer decides the current `FooterMode` and hint flags, then calls
//!    `single_line_footer_layout` for the base single-line modes.
//! 2. `single_line_footer_layout` applies the width-based fallback rules:
//!    (If this description is hard to follow, just try it out by resizing
//!    your terminal width; these rules were built out of trial and error.)
//!    - Start with the fullest left-side hint plus the right-side context.
//!    - When the queue hint is active, prefer keeping that queue hint visible,
//!      even if it means dropping the right-side context earlier; the queue
//!      hint may also be shortened before it is removed.
//!    - Fall back to no left-side footer if nothing can fit.
//! 3. When collapse chooses a specific line, callers render it via
//!    `render_footer_line`. Otherwise, callers render the straightforward
//!    mode-to-text mapping via `render_footer_from_props`.
//!
//! In short: `single_line_footer_layout` chooses *what* best fits, and the two
//! render helpers choose whether to draw the chosen line or the default
//! `FooterProps` mapping.
use crate::key_hint;
use crate::key_hint::KeyBinding;
use crate::key_hint::ShortcutHint;
use crate::render::line_utils::prefix_lines;
#[cfg(test)]
use crate::status::format_tokens_compact;
use crate::ui_consts::FOOTER_INDENT_COLS;
use crossterm::event::KeyCode;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Styled;
use ratatui::style::Stylize;
use ratatui::text::Line;
use ratatui::text::Span;
use ratatui::widgets::Paragraph;
use ratatui::widgets::Widget;

/// The rendering inputs for the footer area under the composer.
///
/// Callers are expected to construct `FooterProps` from higher-level state (`ChatComposer`,
/// `BottomPane`, and `ChatWidget`) and pass it to the footer render helpers
/// (`render_footer_from_props` or the single-line collapse logic). The footer
/// treats these values as authoritative and does not attempt to infer missing
/// state (for example, it does not query whether a task is running).
#[derive(Clone, Debug)]
pub(crate) struct FooterProps {
    pub(crate) mode: FooterMode,
    pub(crate) esc_backtrack_hint: bool,
    pub(crate) use_shift_enter_hint: bool,
    pub(crate) is_task_running: bool,
    pub(crate) queue_submissions: bool,
    /// Which key the user must press again to quit.
    ///
    /// This is rendered when `mode` is `FooterMode::QuitShortcutReminder`.
    pub(crate) quit_shortcut_key: KeyBinding,
    pub(crate) status_line_value: Option<Line<'static>>,
    pub(crate) status_line_enabled: bool,
    pub(crate) key_hints: FooterKeyHints,
    /// Active thread label shown when the footer is rendering contextual information instead of an
    /// instructional hint.
    ///
    /// When both this label and the configured status line are available, they are rendered on the
    /// same row separated by ` · `.
    pub(crate) active_agent_label: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum GoalStatusIndicator {
    Active { usage: Option<String> },
    Paused,
    Blocked,
    UsageLimited,
    BudgetLimited { usage: Option<String> },
    Complete { usage: Option<String> },
}

const FOOTER_CONTEXT_GAP_COLS: u16 = 1;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct FooterKeyHints {
    pub(crate) toggle_shortcuts: Option<ShortcutHint>,
    pub(crate) queue: Option<ShortcutHint>,
    pub(crate) insert_newline: Option<ShortcutHint>,
    pub(crate) external_editor: Option<ShortcutHint>,
    pub(crate) edit_previous: Option<ShortcutHint>,
    pub(crate) show_transcript: Option<ShortcutHint>,
    pub(crate) history_search: Option<ShortcutHint>,
    pub(crate) reasoning_down: Option<ShortcutHint>,
    pub(crate) reasoning_up: Option<ShortcutHint>,
}

impl FooterKeyHints {
    #[cfg(test)]
    pub(crate) fn default_bindings() -> Self {
        Self {
            toggle_shortcuts: Some(key_hint::plain(KeyCode::Char('?')).into()),
            queue: Some(key_hint::plain(KeyCode::Tab).into()),
            insert_newline: Some(key_hint::ctrl(KeyCode::Char('j')).into()),
            external_editor: Some(key_hint::ctrl(KeyCode::Char('g')).into()),
            edit_previous: Some(key_hint::plain(KeyCode::Esc).into()),
            show_transcript: Some(key_hint::ctrl(KeyCode::Char('t')).into()),
            history_search: Some(key_hint::ctrl(KeyCode::Char('r')).into()),
            reasoning_down: Some(key_hint::alt(KeyCode::Char(',')).into()),
            reasoning_up: Some(key_hint::alt(KeyCode::Char('.')).into()),
        }
    }
}

/// Selects which footer content is rendered.
///
/// The current mode is owned by `ChatComposer`, which may override it based on transient state
/// (for example, showing `QuitShortcutReminder` only while its timer is active).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum FooterMode {
    /// Single-line incremental history search prompt shown while Ctrl+R search is active.
    HistorySearch,
    /// Transient "press again to quit" reminder (Ctrl+C/Ctrl+D).
    QuitShortcutReminder,
    /// Multi-line shortcut overlay shown after pressing `?`.
    ShortcutOverlay,
    /// Transient "press Esc again" hint shown after the first Esc while idle.
    EscHint,
    /// Base single-line footer when the composer is empty.
    ComposerEmpty,
    /// Base single-line footer when the composer contains a draft.
    ///
    /// The shortcuts hint is suppressed here; when a task is running, this
    /// mode can show the queue hint instead.
    ComposerHasDraft,
}

pub(crate) fn toggle_shortcut_mode(
    current: FooterMode,
    ctrl_c_hint: bool,
    is_empty: bool,
) -> FooterMode {
    if ctrl_c_hint && matches!(current, FooterMode::QuitShortcutReminder) {
        return current;
    }

    let base_mode = if is_empty {
        FooterMode::ComposerEmpty
    } else {
        FooterMode::ComposerHasDraft
    };

    match current {
        FooterMode::ShortcutOverlay | FooterMode::QuitShortcutReminder => base_mode,
        _ => FooterMode::ShortcutOverlay,
    }
}

pub(crate) fn esc_hint_mode(current: FooterMode, is_task_running: bool) -> FooterMode {
    if is_task_running {
        current
    } else {
        FooterMode::EscHint
    }
}

pub(crate) fn reset_mode_after_activity(current: FooterMode) -> FooterMode {
    match current {
        FooterMode::EscHint
        | FooterMode::ShortcutOverlay
        | FooterMode::QuitShortcutReminder
        | FooterMode::HistorySearch
        | FooterMode::ComposerHasDraft => FooterMode::ComposerEmpty,
        other => other,
    }
}

pub(crate) fn footer_height(props: &FooterProps) -> u16 {
    let show_shortcuts_hint = match props.mode {
        FooterMode::ComposerEmpty => true,
        FooterMode::ComposerHasDraft => false,
        FooterMode::HistorySearch
        | FooterMode::QuitShortcutReminder
        | FooterMode::ShortcutOverlay
        | FooterMode::EscHint => false,
    };
    let show_queue_hint = match props.mode {
        FooterMode::ComposerHasDraft => props.is_task_running,
        FooterMode::QuitShortcutReminder
        | FooterMode::HistorySearch
        | FooterMode::ComposerEmpty
        | FooterMode::ShortcutOverlay
        | FooterMode::EscHint => false,
    };
    footer_from_props_lines(props, show_shortcuts_hint, show_queue_hint).len() as u16
}

/// Render a single precomputed footer line.
pub(crate) fn render_footer_line(area: Rect, buf: &mut Buffer, line: Line<'static>) {
    Paragraph::new(prefix_lines(
        vec![line],
        " ".repeat(FOOTER_INDENT_COLS).into(),
        " ".repeat(FOOTER_INDENT_COLS).into(),
    ))
    .render(area, buf);
}

/// Render footer content directly from `FooterProps`.
///
/// This is intentionally not part of the width-based collapse/fallback logic.
/// Transient instructional states (shortcut overlay, Esc hint, quit reminder)
/// prioritize "what to do next" instructions and currently suppress the
/// agent settings label entirely. When collapse logic has already chosen a
/// specific single line, prefer `render_footer_line`.
pub(crate) fn render_footer_from_props(
    area: Rect,
    buf: &mut Buffer,
    props: &FooterProps,
    show_shortcuts_hint: bool,
    show_queue_hint: bool,
) {
    Paragraph::new(prefix_lines(
        footer_from_props_lines(props, show_shortcuts_hint, show_queue_hint),
        " ".repeat(FOOTER_INDENT_COLS).into(),
        " ".repeat(FOOTER_INDENT_COLS).into(),
    ))
    .render(area, buf);
}

pub(crate) fn left_fits(area: Rect, left_width: u16) -> bool {
    let max_width = area.width.saturating_sub(FOOTER_INDENT_COLS as u16);
    left_width <= max_width
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SummaryHintKind {
    None,
    Shortcuts,
    QueueMessage,
    QueueShort,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct LeftSideState {
    hint: SummaryHintKind,
}

fn left_side_line(state: LeftSideState, key_hints: FooterKeyHints) -> Line<'static> {
    let mut line = Line::from("");
    match state.hint {
        SummaryHintKind::None => {}
        SummaryHintKind::Shortcuts => {
            if let Some(key) = key_hints.toggle_shortcuts {
                line.push_span(key);
                line.push_span(" for shortcuts".dim());
            }
        }
        SummaryHintKind::QueueMessage => {
            if let Some(key) = key_hints.queue {
                line.push_span(key);
                line.push_span(" to queue message".dim());
            }
        }
        SummaryHintKind::QueueShort => {
            if let Some(key) = key_hints.queue {
                line.push_span(key);
                line.push_span(" to queue".dim());
            }
        }
    };

    line
}

pub(crate) enum SummaryLeft {
    Default,
    Custom(Line<'static>),
    None,
}

/// Compute the single-line footer layout and whether the right-side context
/// indicator can be shown alongside it.
pub(crate) fn single_line_footer_layout(
    area: Rect,
    context_width: u16,
    show_shortcuts_hint: bool,
    show_queue_hint: bool,
    key_hints: FooterKeyHints,
) -> (SummaryLeft, bool) {
    let hint_kind = if show_queue_hint {
        SummaryHintKind::QueueMessage
    } else if show_shortcuts_hint {
        SummaryHintKind::Shortcuts
    } else {
        SummaryHintKind::None
    };
    let default_state = LeftSideState { hint: hint_kind };
    let default_line = left_side_line(default_state, key_hints);
    let default_width = default_line.width() as u16;
    if default_width > 0 && can_show_left_with_context(area, default_width, context_width) {
        return (SummaryLeft::Default, true);
    }

    let state_line = |state: LeftSideState| -> Line<'static> {
        if state == default_state {
            default_line.clone()
        } else {
            left_side_line(state, key_hints)
        }
    };
    let state_width = |state: LeftSideState| -> u16 { state_line(state).width() as u16 };
    if show_queue_hint {
        // In queue mode, prefer dropping context before dropping the queue hint.
        let queue_states = [
            default_state,
            LeftSideState {
                hint: SummaryHintKind::QueueMessage,
            },
            LeftSideState {
                hint: SummaryHintKind::QueueShort,
            },
        ];

        // Pass 1: keep the right-side context indicator if any queue variant
        // can fit alongside it. We skip adjacent duplicates because
        // `default_state` can already be the no-cycle queue variant.
        let mut previous_state: Option<LeftSideState> = None;
        for state in queue_states {
            if previous_state == Some(state) {
                continue;
            }
            previous_state = Some(state);
            let width = state_width(state);
            if width > 0 && can_show_left_with_context(area, width, context_width) {
                if state == default_state {
                    return (SummaryLeft::Default, true);
                }
                return (SummaryLeft::Custom(state_line(state)), true);
            }
        }

        // Pass 2: if context cannot fit, drop it before dropping the queue
        // hint. Reuse the same dedupe so we do not try equivalent states twice.
        let mut previous_state: Option<LeftSideState> = None;
        for state in queue_states {
            if previous_state == Some(state) {
                continue;
            }
            previous_state = Some(state);
            let width = state_width(state);
            if width > 0 && left_fits(area, width) {
                if state == default_state {
                    return (SummaryLeft::Default, false);
                }
                return (SummaryLeft::Custom(state_line(state)), false);
            }
        }
    }

    (SummaryLeft::None, true)
}

pub(crate) fn goal_status_indicator_line(
    indicator: Option<&GoalStatusIndicator>,
) -> Option<Line<'static>> {
    let indicator = indicator?;
    let label = match indicator {
        GoalStatusIndicator::Active { usage } => {
            if let Some(usage) = usage {
                format!("Pursuing goal ({usage})")
            } else {
                "Pursuing goal".to_string()
            }
        }
        GoalStatusIndicator::Paused => "Goal paused (/goal resume)".to_string(),
        GoalStatusIndicator::Blocked => "Goal stalled (/goal resume)".to_string(),
        GoalStatusIndicator::UsageLimited => "Goal hit usage limits (/goal resume)".to_string(),
        GoalStatusIndicator::BudgetLimited { usage } => {
            if let Some(usage) = usage {
                format!("Goal unmet ({usage})")
            } else {
                "Goal abandoned".to_string()
            }
        }
        GoalStatusIndicator::Complete { usage } => {
            if let Some(usage) = usage {
                format!("Goal achieved ({usage})")
            } else {
                "Goal achieved".to_string()
            }
        }
    };

    Some(Line::from(vec![Span::from(label).magenta()]))
}

pub(crate) fn status_line_right_indicator_line(
    goal_status_indicator: Option<&GoalStatusIndicator>,
    ide_context_active: bool,
) -> Option<Line<'static>> {
    let primary_indicator = goal_status_indicator_line(goal_status_indicator);
    let ide_context_indicator = ide_context_active.then(|| {
        Line::from(vec![
            "IDE context".set_style(crate::style::operational_reference_style()),
        ])
    });
    let mut line: Option<Line<'static>> = None;

    for indicator in [primary_indicator, ide_context_indicator]
        .into_iter()
        .flatten()
    {
        if let Some(line) = line.as_mut() {
            line.push_span(" · ".dim());
            for span in indicator.spans {
                line.push_span(span);
            }
        } else {
            line = Some(indicator);
        }
    }

    line
}

pub(crate) fn side_conversation_context_line(label: &str) -> Line<'static> {
    if let Some(rest) = label.strip_prefix("Side ") {
        Line::from(vec!["Side".magenta().bold(), format!(" {rest}").magenta()])
    } else {
        Line::from(vec![Span::from(label.to_string()).magenta()])
    }
}

fn right_aligned_x(area: Rect, content_width: u16) -> Option<u16> {
    if area.is_empty() {
        return None;
    }

    let right_padding = FOOTER_INDENT_COLS as u16;
    let max_width = area.width.saturating_sub(right_padding);
    if content_width == 0 || max_width == 0 {
        return None;
    }

    if content_width >= max_width {
        return Some(area.x.saturating_add(right_padding));
    }

    Some(
        area.x
            .saturating_add(area.width)
            .saturating_sub(content_width)
            .saturating_sub(right_padding),
    )
}

pub(crate) fn max_left_width_for_right(area: Rect, right_width: u16) -> Option<u16> {
    let context_x = right_aligned_x(area, right_width)?;
    let left_start = area.x + FOOTER_INDENT_COLS as u16;

    // minimal one column gap between left and right
    let gap = FOOTER_CONTEXT_GAP_COLS;

    if context_x <= left_start + gap {
        return Some(0);
    }

    Some(context_x.saturating_sub(left_start + gap))
}

pub(crate) fn can_show_left_with_context(area: Rect, left_width: u16, context_width: u16) -> bool {
    let Some(context_x) = right_aligned_x(area, context_width) else {
        return true;
    };
    if left_width == 0 {
        return true;
    }
    let left_extent = FOOTER_INDENT_COLS as u16 + left_width + FOOTER_CONTEXT_GAP_COLS;
    left_extent <= context_x.saturating_sub(area.x)
}

pub(crate) fn render_context_right(area: Rect, buf: &mut Buffer, line: &Line<'static>) {
    if area.is_empty() {
        return;
    }

    let context_width = line.width() as u16;
    let Some(mut x) = right_aligned_x(area, context_width) else {
        return;
    };
    let y = area.y + area.height.saturating_sub(1);
    let max_x = area.x.saturating_add(area.width);

    for span in &line.spans {
        if x >= max_x {
            break;
        }
        let span_width = span.width() as u16;
        if span_width == 0 {
            continue;
        }
        let remaining = max_x.saturating_sub(x);
        let draw_width = span_width.min(remaining);
        buf.set_span(x, y, span, draw_width);
        x = x.saturating_add(span_width);
    }
}

pub(crate) fn inset_footer_hint_area(mut area: Rect) -> Rect {
    if area.width > 2 {
        area.x += 2;
        area.width = area.width.saturating_sub(2);
    }
    area
}

pub(crate) fn render_footer_hint_items(area: Rect, buf: &mut Buffer, items: &[(String, String)]) {
    if items.is_empty() {
        return;
    }

    footer_hint_items_line(items).render(inset_footer_hint_area(area), buf);
}

/// Map `FooterProps` to footer lines without width-based collapse.
///
/// This is the canonical FooterMode-to-text mapping. It powers transient,
/// instructional states (shortcut overlay, Esc hint, quit reminder) and also
/// the default rendering for base states when collapse is not applied (or when
/// `single_line_footer_layout` returns `SummaryLeft::Default`). Collapse and
/// fallback decisions live in `single_line_footer_layout`; this function only
/// formats the chosen/default content.
fn footer_from_props_lines(
    props: &FooterProps,
    _show_shortcuts_hint: bool,
    show_queue_hint: bool,
) -> Vec<Line<'static>> {
    let key_hints = props.key_hints;
    // Passive footer context can come from the configurable status line, the
    // active agent label, or both combined.
    if let Some(status_line) = passive_footer_status_line(props) {
        return vec![status_line];
    }
    match props.mode {
        FooterMode::QuitShortcutReminder => {
            vec![quit_shortcut_reminder_line(props.quit_shortcut_key)]
        }
        FooterMode::HistorySearch => vec![Line::from("reverse-i-search: ").dim()],
        FooterMode::ComposerEmpty => Vec::new(),
        FooterMode::ShortcutOverlay => {
            let state = ShortcutsState {
                use_shift_enter_hint: props.use_shift_enter_hint,
                esc_backtrack_hint: props.esc_backtrack_hint,
                is_task_running: props.is_task_running,
                queue_submissions: props.queue_submissions,
                key_hints,
            };
            shortcut_overlay_lines(state)
        }
        FooterMode::EscHint => vec![esc_hint_line(props.esc_backtrack_hint)],
        FooterMode::ComposerHasDraft => {
            if !show_queue_hint {
                return Vec::new();
            }
            let state = LeftSideState {
                hint: SummaryHintKind::QueueMessage,
            };
            vec![left_side_line(state, key_hints)]
        }
    }
}

/// Returns the contextual footer row when the footer is not busy showing an instructional hint.
///
/// The returned line may contain the configured status line, the currently viewed agent label, or
/// both combined. Active instructional states such as quit reminders, shortcut overlays, and queue
/// prompts deliberately return `None` so those call-to-action hints stay visible.
pub(crate) fn passive_footer_status_line(props: &FooterProps) -> Option<Line<'static>> {
    if !shows_passive_footer_line(props) {
        return None;
    }

    let mut line = if props.status_line_enabled {
        props.status_line_value.clone()
    } else {
        None
    };

    if let Some(active_agent_label) = props.active_agent_label.as_ref() {
        if let Some(existing) = line.as_mut() {
            existing.spans.push(" · ".dim());
            existing.spans.push(active_agent_label.clone().dim());
        } else {
            line = Some(Line::from(active_agent_label.clone()).dim());
        }
    }

    line
}

/// Whether the current footer mode allows contextual information to replace instructional hints.
///
/// In practice this means the composer is idle, or it has a draft but is not currently running a
/// task, so the footer can spend the row on ambient context instead of "what to do next" text.
pub(crate) fn shows_passive_footer_line(props: &FooterProps) -> bool {
    match props.mode {
        FooterMode::ComposerEmpty => true,
        FooterMode::ComposerHasDraft => !props.is_task_running,
        FooterMode::HistorySearch
        | FooterMode::QuitShortcutReminder
        | FooterMode::ShortcutOverlay
        | FooterMode::EscHint => false,
    }
}

/// Whether callers should reserve the dedicated status-line layout for a contextual footer row.
///
/// The dedicated layout exists for the configurable `/statusline` row. An agent label by itself
/// can be rendered by the standard footer flow, so this only becomes `true` when the status line
/// feature is enabled and the current mode allows contextual footer content.
pub(crate) fn uses_passive_footer_status_layout(props: &FooterProps) -> bool {
    props.status_line_enabled && shows_passive_footer_line(props)
}

pub(crate) fn footer_line_width(
    props: &FooterProps,
    show_shortcuts_hint: bool,
    show_queue_hint: bool,
) -> u16 {
    footer_from_props_lines(props, show_shortcuts_hint, show_queue_hint)
        .last()
        .map(|line| line.width() as u16)
        .unwrap_or(0)
}

pub(crate) fn footer_hint_items_width(items: &[(String, String)]) -> u16 {
    if items.is_empty() {
        return 0;
    }
    footer_hint_items_line(items).width() as u16
}

fn footer_hint_items_line(items: &[(String, String)]) -> Line<'static> {
    let mut spans = Vec::with_capacity(items.len() * 4);
    for (idx, (key, label)) in items.iter().enumerate() {
        spans.push(" ".into());
        spans.push(key.clone().bold());
        spans.push(format!(" {label}").into());
        if idx + 1 != items.len() {
            spans.push("   ".into());
        }
    }
    Line::from(spans)
}

#[derive(Clone, Copy, Debug)]
struct ShortcutsState {
    use_shift_enter_hint: bool,
    esc_backtrack_hint: bool,
    is_task_running: bool,
    queue_submissions: bool,
    key_hints: FooterKeyHints,
}

fn quit_shortcut_reminder_line(key: KeyBinding) -> Line<'static> {
    Line::from(vec![key.into(), " again to quit".into()]).dim()
}

fn esc_hint_line(esc_backtrack_hint: bool) -> Line<'static> {
    let esc = key_hint::plain(KeyCode::Esc);
    if esc_backtrack_hint {
        Line::from(vec![esc.into(), " again to edit previous message".into()]).dim()
    } else {
        Line::from(vec![
            esc.into(),
            " ".into(),
            esc.into(),
            " to edit previous message".into(),
        ])
        .dim()
    }
}

fn shortcut_overlay_lines(state: ShortcutsState) -> Vec<Line<'static>> {
    let mut commands = Line::from("");
    let mut shell_commands = Line::from("");
    let mut newline = Line::from("");
    let mut queue_message_tab = Line::from("");
    let mut file_paths = Line::from("");
    let mut paste_image = Line::from("");
    let mut external_editor = Line::from("");
    let mut edit_previous = Line::from("");
    let mut history_search = Line::from("");
    let mut quit = Line::from("");
    let mut show_transcript = Line::from("");
    let mut reasoning_down = Line::from("");
    let mut reasoning_up = Line::from("");

    for descriptor in SHORTCUTS {
        if let Some(text) = descriptor.overlay_entry(state) {
            match descriptor.id {
                ShortcutId::Commands => commands = text,
                ShortcutId::ShellCommands => shell_commands = text,
                ShortcutId::InsertNewline => newline = text,
                ShortcutId::QueueMessageTab => queue_message_tab = text,
                ShortcutId::FilePaths => file_paths = text,
                ShortcutId::PasteImage => paste_image = text,
                ShortcutId::ExternalEditor => external_editor = text,
                ShortcutId::EditPrevious => edit_previous = text,
                ShortcutId::HistorySearch => history_search = text,
                ShortcutId::Quit => quit = text,
                ShortcutId::ShowTranscript => show_transcript = text,
                ShortcutId::ReasoningDown => reasoning_down = text,
                ShortcutId::ReasoningUp => reasoning_up = text,
            }
        }
    }

    let mut ordered = vec![
        commands,
        shell_commands,
        newline,
        queue_message_tab,
        file_paths,
        paste_image,
        external_editor,
        edit_previous,
        history_search,
        quit,
        reasoning_down,
        reasoning_up,
    ];
    ordered.push(show_transcript);

    let mut lines = build_columns(ordered);
    lines.push(Line::from(""));
    lines.push(Line::from(vec![
        "customize shortcuts with ".into(),
        "/keymap".set_style(crate::style::operational_reference_style()),
    ]));
    lines
}

fn build_columns(entries: Vec<Line<'static>>) -> Vec<Line<'static>> {
    if entries.is_empty() {
        return Vec::new();
    }

    const COLUMNS: usize = 2;
    const COLUMN_PADDING: [usize; COLUMNS] = [4, 4];
    const COLUMN_GAP: usize = 4;

    let rows = entries.len().div_ceil(COLUMNS);
    let target_len = rows * COLUMNS;
    let mut entries = entries;
    if entries.len() < target_len {
        entries.extend(std::iter::repeat_n(
            Line::from(""),
            target_len - entries.len(),
        ));
    }

    let mut column_widths = [0usize; COLUMNS];

    for (idx, entry) in entries.iter().enumerate() {
        let column = idx % COLUMNS;
        column_widths[column] = column_widths[column].max(entry.width());
    }

    for (idx, width) in column_widths.iter_mut().enumerate() {
        *width += COLUMN_PADDING[idx];
    }

    entries
        .chunks(COLUMNS)
        .map(|chunk| {
            let mut line = Line::from("");
            for (col, entry) in chunk.iter().enumerate() {
                line.extend(entry.spans.clone());
                if col < COLUMNS - 1 {
                    let target_width = column_widths[col];
                    let padding = target_width.saturating_sub(entry.width()) + COLUMN_GAP;
                    line.push_span(Span::from(" ".repeat(padding)));
                }
            }
            line.dim()
        })
        .collect()
}

#[cfg(test)]
pub(crate) fn context_window_line(percent: Option<i64>, used_tokens: Option<i64>) -> Line<'static> {
    if let Some(percent) = percent {
        let percent = percent.clamp(0, 100);
        return Line::from(vec![Span::from(format!("{percent}% context left")).dim()]);
    }

    if let Some(tokens) = used_tokens {
        let used_fmt = format_tokens_compact(tokens);
        return Line::from(vec![Span::from(format!("{used_fmt} used")).dim()]);
    }

    Line::from(vec![Span::from("100% context left").dim()])
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ShortcutId {
    Commands,
    ShellCommands,
    InsertNewline,
    QueueMessageTab,
    FilePaths,
    PasteImage,
    ExternalEditor,
    EditPrevious,
    HistorySearch,
    Quit,
    ShowTranscript,
    ReasoningDown,
    ReasoningUp,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ShortcutBinding {
    key: KeyBinding,
    condition: DisplayCondition,
}

impl ShortcutBinding {
    fn matches(&self, state: ShortcutsState) -> bool {
        self.condition.matches(state)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DisplayCondition {
    Always,
    WhenShiftEnterHint,
    WhenNotShiftEnterHint,
}

impl DisplayCondition {
    fn matches(self, state: ShortcutsState) -> bool {
        match self {
            DisplayCondition::Always => true,
            DisplayCondition::WhenShiftEnterHint => state.use_shift_enter_hint,
            DisplayCondition::WhenNotShiftEnterHint => !state.use_shift_enter_hint,
        }
    }
}

struct ShortcutDescriptor {
    id: ShortcutId,
    bindings: &'static [ShortcutBinding],
    prefix: &'static str,
    label: &'static str,
}

impl ShortcutDescriptor {
    fn binding_for(&self, state: ShortcutsState) -> Option<&'static ShortcutBinding> {
        self.bindings.iter().find(|binding| binding.matches(state))
    }

    fn overlay_entry(&self, state: ShortcutsState) -> Option<Line<'static>> {
        let key = match self.id {
            ShortcutId::InsertNewline => state.key_hints.insert_newline,
            ShortcutId::QueueMessageTab => state.key_hints.queue,
            ShortcutId::ExternalEditor => state.key_hints.external_editor,
            ShortcutId::EditPrevious => state.key_hints.edit_previous,
            ShortcutId::ShowTranscript => state.key_hints.show_transcript,
            ShortcutId::HistorySearch => state.key_hints.history_search,
            ShortcutId::ReasoningDown => state.key_hints.reasoning_down,
            ShortcutId::ReasoningUp => state.key_hints.reasoning_up,
            ShortcutId::Commands
            | ShortcutId::ShellCommands
            | ShortcutId::FilePaths
            | ShortcutId::PasteImage
            | ShortcutId::Quit => self
                .binding_for(state)
                .map(|binding| ShortcutHint::Single(binding.key)),
        }?;
        let mut line = Line::from(vec![self.prefix.into(), key.into()]);
        match self.id {
            ShortcutId::QueueMessageTab => {
                if state.is_task_running || state.queue_submissions {
                    line.push_span(" to queue message");
                } else {
                    line.push_span(" to submit message");
                }
            }
            ShortcutId::EditPrevious => {
                if state.esc_backtrack_hint {
                    line.push_span(" again to edit previous message");
                } else {
                    line.extend(vec![
                        " ".into(),
                        key.into(),
                        " to edit previous message".into(),
                    ]);
                }
            }
            ShortcutId::Quit => {
                if state.is_task_running {
                    line.push_span(" to interrupt");
                } else {
                    line.push_span(" to exit");
                }
            }
            _ => line.push_span(self.label),
        };
        Some(line)
    }
}

const SHORTCUTS: &[ShortcutDescriptor] = &[
    ShortcutDescriptor {
        id: ShortcutId::Commands,
        bindings: &[ShortcutBinding {
            key: key_hint::plain(KeyCode::Char('/')),
            condition: DisplayCondition::Always,
        }],
        prefix: "",
        label: " for commands",
    },
    ShortcutDescriptor {
        id: ShortcutId::ShellCommands,
        bindings: &[ShortcutBinding {
            key: key_hint::plain(KeyCode::Char('!')),
            condition: DisplayCondition::Always,
        }],
        prefix: "",
        label: " for shell commands",
    },
    ShortcutDescriptor {
        id: ShortcutId::InsertNewline,
        bindings: &[
            ShortcutBinding {
                key: key_hint::shift(KeyCode::Enter),
                condition: DisplayCondition::WhenShiftEnterHint,
            },
            ShortcutBinding {
                key: key_hint::ctrl(KeyCode::Char('j')),
                condition: DisplayCondition::WhenNotShiftEnterHint,
            },
        ],
        prefix: "",
        label: " for newline",
    },
    ShortcutDescriptor {
        id: ShortcutId::QueueMessageTab,
        bindings: &[ShortcutBinding {
            key: key_hint::plain(KeyCode::Tab),
            condition: DisplayCondition::Always,
        }],
        prefix: "",
        label: " to queue message",
    },
    ShortcutDescriptor {
        id: ShortcutId::FilePaths,
        bindings: &[ShortcutBinding {
            key: key_hint::plain(KeyCode::Char('@')),
            condition: DisplayCondition::Always,
        }],
        prefix: "",
        label: " for file paths",
    },
    ShortcutDescriptor {
        id: ShortcutId::PasteImage,
        bindings: &[ShortcutBinding {
            key: key_hint::ctrl(KeyCode::Char('v')),
            condition: DisplayCondition::Always,
        }],
        prefix: "",
        label: " to paste images",
    },
    ShortcutDescriptor {
        id: ShortcutId::ExternalEditor,
        bindings: &[ShortcutBinding {
            key: key_hint::ctrl(KeyCode::Char('g')),
            condition: DisplayCondition::Always,
        }],
        prefix: "",
        label: " to edit in external editor",
    },
    ShortcutDescriptor {
        id: ShortcutId::EditPrevious,
        bindings: &[ShortcutBinding {
            key: key_hint::plain(KeyCode::Esc),
            condition: DisplayCondition::Always,
        }],
        prefix: "",
        label: "",
    },
    ShortcutDescriptor {
        id: ShortcutId::HistorySearch,
        bindings: &[ShortcutBinding {
            key: key_hint::ctrl(KeyCode::Char('r')),
            condition: DisplayCondition::Always,
        }],
        prefix: "",
        label: " search history",
    },
    ShortcutDescriptor {
        id: ShortcutId::Quit,
        bindings: &[ShortcutBinding {
            key: key_hint::ctrl(KeyCode::Char('c')),
            condition: DisplayCondition::Always,
        }],
        prefix: "",
        label: " to exit",
    },
    ShortcutDescriptor {
        id: ShortcutId::ShowTranscript,
        bindings: &[ShortcutBinding {
            key: key_hint::ctrl(KeyCode::Char('t')),
            condition: DisplayCondition::Always,
        }],
        prefix: "",
        label: " to view transcript",
    },
    ShortcutDescriptor {
        id: ShortcutId::ReasoningDown,
        bindings: &[ShortcutBinding {
            key: key_hint::alt(KeyCode::Char(',')),
            condition: DisplayCondition::Always,
        }],
        prefix: "",
        label: " reasoning down",
    },
    ShortcutDescriptor {
        id: ShortcutId::ReasoningUp,
        bindings: &[ShortcutBinding {
            key: key_hint::alt(KeyCode::Char('.')),
            condition: DisplayCondition::Always,
        }],
        prefix: "",
        label: " reasoning up",
    },
];

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn single_line_footer_keeps_shortcut_hint_with_context_when_it_fits() {
        let (left, show_context) = single_line_footer_layout(
            Rect::new(
                /*x*/ 0, /*y*/ 0, /*width*/ 80, /*height*/ 1,
            ),
            /*context_width*/ 12,
            /*show_shortcuts_hint*/ true,
            /*show_queue_hint*/ false,
            FooterKeyHints::default_bindings(),
        );

        assert!(matches!(left, SummaryLeft::Default));
        assert!(show_context);
    }

    #[test]
    fn right_indicator_combines_goal_and_ide_context() {
        let line = status_line_right_indicator_line(
            Some(&GoalStatusIndicator::Active { usage: None }),
            /*ide_context_active*/ true,
        )
        .expect("right indicator");

        assert_eq!(line.to_string(), "Pursuing goal · IDE context");
    }
}
