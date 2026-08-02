//! Out-of-process panels over a small, versioned JSON-lines protocol.
//!
//! Mirador does not discover plugins, embed an interpreter, or load dynamic
//! libraries. A process exists only when the config explicitly declares its
//! command *and* the layout places its id. The process publishes complete
//! render snapshots; Mirador remains the only owner of the real terminal and
//! the only renderer touching ratatui.

use std::sync::mpsc::TrySendError;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ratatui::Frame;
use ratatui::crossterm::event::{
    KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Paragraph};
use serde::{Deserialize, Serialize};

use crate::config::PluginConfig;
use crate::frame::Binding;
use crate::panel::{KeyOutcome, Panel, RenderContext};

mod process;

use process::{Phase, Runtime, Shared, spawn_process};

pub const PROTOCOL_VERSION: u16 = 1;
const MAX_MESSAGE_BYTES: usize = 8 * 1024 * 1024;
const MAX_WATCH_TEXT_BYTES: usize = 1024;
const DEFAULT_REFRESH: Duration = Duration::from_millis(33);

#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum HostMessage {
    Hello {
        protocol: u16,
        host_version: &'static str,
        plugin: String,
        config: serde_json::Value,
        cwd: String,
    },
    Resize {
        columns: u16,
        rows: u16,
    },
    Focus {
        focused: bool,
    },
    Key {
        key: String,
        code: String,
        text: Option<String>,
        modifiers: Vec<String>,
    },
    Interrupt,
    Paste {
        text: String,
    },
    Mouse {
        kind: String,
        button: Option<String>,
        column: u16,
        row: u16,
        modifiers: Vec<String>,
    },
    Tick,
    Shutdown,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum PluginMessage {
    Ready {
        protocol: u16,
        #[serde(default)]
        title: Option<String>,
        #[serde(default)]
        refresh_ms: Option<u64>,
    },
    Frame {
        revision: u64,
        #[serde(default)]
        title: Option<String>,
        #[serde(default)]
        counter: Option<String>,
        #[serde(default)]
        lines: Vec<WireLine>,
        #[serde(default)]
        bindings: Vec<WireBinding>,
        #[serde(default)]
        input: InputPolicy,
        #[serde(default)]
        cursor: Option<WireCursor>,
    },
    Error {
        message: String,
        #[serde(default)]
        fatal: bool,
    },
    Watch {
        text: String,
    },
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct WireLine {
    #[serde(default)]
    spans: Vec<WireSpan>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
// These are independent terminal style attributes on the wire, not states of
// one machine; combining them would make the protocol less direct.
#[allow(clippy::struct_excessive_bools)]
struct WireSpan {
    text: String,
    #[serde(default)]
    fg: Option<String>,
    #[serde(default)]
    bg: Option<String>,
    #[serde(default)]
    bold: bool,
    #[serde(default)]
    dim: bool,
    #[serde(default)]
    italic: bool,
    #[serde(default)]
    underlined: bool,
    #[serde(default)]
    reversed: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct WireBinding {
    key: String,
    action: String,
    #[serde(default)]
    primary: bool,
}

#[derive(Debug, Clone, Deserialize, Default, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
// Each flag grants a distinct input class. A bitset would leak a Rust encoding
// into a language-neutral JSON protocol.
#[allow(clippy::struct_excessive_bools)]
struct InputPolicy {
    /// Consume every key while focused.
    capture: bool,
    /// Canonical keys to consume while not capturing, e.g. `Enter` or `r`.
    keys: Vec<String>,
    /// Consume one Ctrl+C, then disarm until other input arrives.
    interrupt: bool,
    paste: bool,
    mouse: bool,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(deny_unknown_fields)]
struct WireCursor {
    column: u16,
    row: u16,
    #[serde(default = "yes")]
    visible: bool,
}

const fn yes() -> bool {
    true
}

#[derive(Debug, Clone)]
struct WireFrame {
    revision: u64,
    title: Option<String>,
    counter: Option<String>,
    lines: Vec<WireLine>,
    bindings: Vec<WireBinding>,
    input: InputPolicy,
    cursor: Option<WireCursor>,
}

/// Adapter from one explicitly configured process to Mirador's private panel
/// trait. Process failures are panel state, never application failures.
pub struct PluginPanel {
    spec: PluginConfig,
    runtime: Option<Runtime>,
    shared: Arc<Mutex<Shared>>,
    seen_generation: u64,
    phase: Phase,
    title: String,
    refresh: Duration,
    frame: Option<WireFrame>,
    bindings: Vec<Binding>,
    policy: InputPolicy,
    notice: Option<String>,
    /// Revision whose passive key action is awaiting a new frame. Until that
    /// acknowledgement arrives, subsequent input is conservatively captured
    /// so a rapid `Enter`, `q` cannot cross an asynchronous mode transition.
    input_barrier: Option<u64>,
    interrupt_armed: bool,
    last_size: Option<(u16, u16)>,
    last_focus: Option<bool>,
}

impl PluginPanel {
    pub fn new(spec: PluginConfig) -> Self {
        let id = spec.id.clone();
        let mut panel = Self {
            spec,
            runtime: None,
            shared: Arc::new(Mutex::new(Shared::starting())),
            seen_generation: u64::MAX,
            phase: Phase::Starting,
            title: id,
            refresh: DEFAULT_REFRESH,
            frame: None,
            bindings: Vec::new(),
            policy: InputPolicy::default(),
            notice: None,
            input_barrier: None,
            interrupt_armed: false,
            last_size: None,
            last_focus: None,
        };
        panel.start();
        panel.sync();
        panel
    }

    fn start(&mut self) {
        self.stop();
        self.shared = Arc::new(Mutex::new(Shared::starting()));
        self.seen_generation = u64::MAX;
        self.phase = Phase::Starting;
        self.frame = None;
        self.bindings.clear();
        self.policy = InputPolicy::default();
        self.notice = None;
        self.input_barrier = None;
        self.interrupt_armed = false;
        self.last_size = None;
        self.last_focus = None;

        match spawn_process(&self.spec, &self.shared) {
            Ok(runtime) => self.runtime = Some(runtime),
            Err(error) => self
                .shared
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .fail(format!("could not start plugin: {error}")),
        }
    }

    fn stop(&mut self) {
        if let Some(runtime) = self.runtime.take() {
            runtime.shutdown();
        }
    }

    fn sync(&mut self) -> bool {
        let shared = self
            .shared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if shared.generation == self.seen_generation {
            return false;
        }

        self.seen_generation = shared.generation;
        self.phase = shared.phase.clone();
        self.refresh = shared.refresh;
        self.notice.clone_from(&shared.notice);
        if let Some(title) = &shared.title {
            self.title.clone_from(title);
        }
        let next = shared.frame.clone();
        drop(shared);

        if let Some(frame) = &next {
            let was_interruptible = self.accepts_interrupt();
            self.policy = frame.input.clone();
            if self
                .input_barrier
                .is_some_and(|revision| frame.revision > revision)
            {
                self.input_barrier = None;
            }
            if matches!(self.phase, Phase::Exited(_) | Phase::Failed(_)) {
                self.input_barrier = None;
            }
            let is_interruptible = self.accepts_interrupt();
            if !was_interruptible && is_interruptible {
                self.interrupt_armed = true;
            } else if !is_interruptible {
                self.interrupt_armed = false;
            }
            self.bindings = frame
                .bindings
                .iter()
                .map(|binding| {
                    Binding::owned(binding.key.clone(), binding.action.clone(), binding.primary)
                })
                .collect();
            if let Some(title) = &frame.title {
                self.title.clone_from(title);
            }
        } else {
            self.policy = InputPolicy::default();
            self.input_barrier = None;
            self.interrupt_armed = false;
            self.bindings.clear();
        }
        if matches!(self.phase, Phase::Exited(_) | Phase::Failed(_)) {
            self.bindings.push(Binding::primary("r", "restart"));
        }
        self.frame = next;
        true
    }

    fn send(&mut self, message: HostMessage) -> bool {
        let Some(runtime) = &self.runtime else {
            return false;
        };
        match runtime.events.try_send(message) {
            Ok(()) => true,
            Err(TrySendError::Full(_)) => {
                let mut shared = self
                    .shared
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if shared.notice.as_deref() != Some("plugin input queue is full") {
                    shared.notice = Some("plugin input queue is full".into());
                    shared.changed();
                }
                false
            }
            Err(TrySendError::Disconnected(_)) => false,
        }
    }

    fn rearm_interrupt(&mut self) {
        self.interrupt_armed = self.accepts_interrupt();
    }

    fn accepts_interrupt(&self) -> bool {
        self.policy.interrupt || self.input_barrier.is_some()
    }

    fn begin_input_barrier(&mut self) {
        if self.policy.capture || self.input_barrier.is_some() {
            return;
        }
        if let Some(revision) = self.frame.as_ref().map(|frame| frame.revision) {
            self.input_barrier = Some(revision);
            self.interrupt_armed = true;
        }
    }

    fn status_lines(&self, theme: &crate::theme::Theme) -> Vec<Line<'static>> {
        let (headline, detail) = match &self.phase {
            Phase::Starting => ("starting plugin".to_string(), None),
            Phase::Running => ("waiting for first frame".to_string(), None),
            Phase::Stopping => ("stopping plugin".to_string(), None),
            Phase::Exited(status) => (
                "plugin exited".to_string(),
                Some(format!("{status}; press r to restart")),
            ),
            Phase::Failed(error) => (
                "plugin failed".to_string(),
                Some(format!("{error}; press r to retry")),
            ),
        };
        let mut lines = vec![Line::from(Span::styled(
            headline,
            Style::default().fg(theme.muted),
        ))];
        if let Some(detail) = detail {
            lines.push(Line::from(Span::styled(
                detail,
                Style::default().fg(theme.error),
            )));
        }
        let shared = self
            .shared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(notice) = &shared.notice {
            lines.push(Line::from(Span::styled(
                notice.clone(),
                Style::default().fg(theme.warning),
            )));
        }
        if !shared.stderr.trim().is_empty() {
            lines.push(Line::from(""));
            lines.extend(shared.stderr.lines().take(3).map(|line| {
                Line::from(Span::styled(
                    line.to_string(),
                    Style::default().fg(theme.muted),
                ))
            }));
        }
        lines
    }
}

impl Panel for PluginPanel {
    fn title(&self) -> String {
        self.title.clone()
    }

    fn counter(&self) -> Option<String> {
        match &self.phase {
            Phase::Starting => Some("starting".into()),
            Phase::Exited(_) => Some("exited".into()),
            Phase::Failed(_) => Some("failed".into()),
            Phase::Running | Phase::Stopping => {
                self.frame.as_ref().and_then(|frame| frame.counter.clone())
            }
        }
    }

    fn bindings(&self) -> &[Binding] {
        &self.bindings
    }

    fn refresh_interval(&self) -> Duration {
        self.refresh
    }

    fn tick(&mut self) -> bool {
        let _ = self.send(HostMessage::Tick);
        self.sync()
    }

    fn events(&mut self) -> Vec<crate::watch::Event> {
        let mut shared = self
            .shared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        shared
            .watch
            .drain(..)
            .map(|text| crate::watch::Event::new(self.spec.id.clone(), text))
            .collect()
    }

    fn render(&mut self, frame: &mut Frame, area: Rect, ctx: RenderContext<'_>) {
        self.sync();
        let size = (area.width.max(1), area.height.max(1));
        if self.last_size != Some(size)
            && self.send(HostMessage::Resize {
                columns: size.0,
                rows: size.1,
            })
        {
            self.last_size = Some(size);
        }
        if self.last_focus != Some(ctx.focused)
            && self.send(HostMessage::Focus {
                focused: ctx.focused,
            })
        {
            self.last_focus = Some(ctx.focused);
        }

        let Some(snapshot) = &self.frame else {
            frame.render_widget(Paragraph::new(self.status_lines(ctx.theme)), area);
            return;
        };

        let lines: Vec<Line<'static>> = snapshot
            .lines
            .iter()
            .take(usize::from(area.height))
            .map(|line| {
                Line::from(
                    line.spans
                        .iter()
                        .map(|span| Span::styled(span.text.clone(), span_style(span, ctx.theme)))
                        .collect::<Vec<_>>(),
                )
            })
            .collect();
        frame.render_widget(
            Paragraph::new(lines).style(Style::default().fg(ctx.theme.text)),
            area,
        );

        let (notice, notice_color) = if let Some(notice) = self.notice.as_deref() {
            (Some(notice), ctx.theme.warning)
        } else {
            (
                match &self.phase {
                    Phase::Exited(status) => Some(status.as_str()),
                    Phase::Failed(error) => Some(error.as_str()),
                    Phase::Starting | Phase::Running | Phase::Stopping => None,
                },
                ctx.theme.error,
            )
        };
        if let Some(notice) = notice
            && area.height > 0
        {
            let status = Rect::new(
                area.x,
                area.y + area.height.saturating_sub(1),
                area.width,
                1,
            );
            frame.render_widget(Clear, status);
            frame.render_widget(
                Paragraph::new(Span::styled(
                    notice.to_string(),
                    Style::default().fg(notice_color),
                )),
                status,
            );
        }

        if ctx.focused
            && let Some(cursor) = snapshot.cursor
            && cursor.visible
            && cursor.column < area.width
            && cursor.row < area.height
        {
            frame.set_cursor_position((area.x + cursor.column, area.y + cursor.row));
        }
    }

    fn handle_key(&mut self, key: KeyEvent) -> KeyOutcome {
        let canonical = canonical_key(key);
        if matches!(self.phase, Phase::Exited(_) | Phase::Failed(_)) && canonical == "r" {
            self.start();
            return KeyOutcome::Consumed;
        }
        if !self.policy.capture
            && self.input_barrier.is_none()
            && !self.policy.keys.iter().any(|item| item == &canonical)
        {
            return KeyOutcome::Ignored;
        }

        self.rearm_interrupt();
        let accepted = self.send(HostMessage::Key {
            key: canonical,
            code: key_code(key.code),
            text: match key.code {
                KeyCode::Char(character) => Some(character.to_string()),
                _ => None,
            },
            modifiers: key_modifiers(key.modifiers),
        });
        if accepted {
            self.begin_input_barrier();
        }
        // Once a plugin declares capture, a stalled process must not turn its
        // queued `q` into Mirador's global quit key.
        KeyOutcome::Consumed
    }

    fn handle_interrupt(&mut self) -> KeyOutcome {
        if !self.accepts_interrupt() || !self.interrupt_armed {
            return KeyOutcome::Ignored;
        }
        self.interrupt_armed = false;
        let _ = self.send(HostMessage::Interrupt);
        KeyOutcome::Consumed
    }

    fn handle_paste(&mut self, text: &str) -> KeyOutcome {
        if !self.policy.paste && self.input_barrier.is_none() {
            return KeyOutcome::Ignored;
        }
        self.rearm_interrupt();
        let _ = self.send(HostMessage::Paste {
            text: text.to_string(),
        });
        KeyOutcome::Consumed
    }

    fn handle_mouse(&mut self, event: MouseEvent, area: Rect) -> KeyOutcome {
        if !self.policy.mouse && self.input_barrier.is_none() {
            return KeyOutcome::Ignored;
        }
        self.rearm_interrupt();
        let (kind, button) = mouse_kind(event.kind);
        let _ = self.send(HostMessage::Mouse {
            kind,
            button,
            column: event.column.saturating_sub(area.x),
            row: event.row.saturating_sub(area.y),
            modifiers: key_modifiers(event.modifiers),
        });
        KeyOutcome::Consumed
    }

    fn captures_input(&self) -> bool {
        self.policy.capture || self.input_barrier.is_some()
    }

    fn shutdown(&mut self) {
        self.stop();
        self.phase = Phase::Stopping;
    }
}

impl Drop for PluginPanel {
    fn drop(&mut self) {
        self.stop();
    }
}

fn canonical_key(key: KeyEvent) -> String {
    let base = match key.code {
        KeyCode::Char(' ') => "Space".into(),
        KeyCode::Char(character) => character.to_string(),
        KeyCode::Enter => "Enter".into(),
        KeyCode::Backspace => "Backspace".into(),
        KeyCode::Tab => "Tab".into(),
        KeyCode::BackTab => "BackTab".into(),
        KeyCode::Esc => "Esc".into(),
        KeyCode::Left => "Left".into(),
        KeyCode::Right => "Right".into(),
        KeyCode::Up => "Up".into(),
        KeyCode::Down => "Down".into(),
        KeyCode::Home => "Home".into(),
        KeyCode::End => "End".into(),
        KeyCode::PageUp => "PageUp".into(),
        KeyCode::PageDown => "PageDown".into(),
        KeyCode::Delete => "Delete".into(),
        KeyCode::Insert => "Insert".into(),
        KeyCode::F(number) => format!("F{number}"),
        other => format!("{other:?}"),
    };
    let mut prefixes = key_modifiers(key.modifiers);
    if prefixes.is_empty() {
        base
    } else {
        prefixes.push(base);
        prefixes.join("+")
    }
}

fn key_code(code: KeyCode) -> String {
    match code {
        KeyCode::Char(_) => "char".into(),
        KeyCode::F(number) => format!("f{number}"),
        other => format!("{other:?}").to_ascii_lowercase(),
    }
}

fn key_modifiers(modifiers: KeyModifiers) -> Vec<String> {
    [
        (KeyModifiers::CONTROL, "Ctrl"),
        (KeyModifiers::ALT, "Alt"),
        (KeyModifiers::SHIFT, "Shift"),
        (KeyModifiers::SUPER, "Super"),
        (KeyModifiers::HYPER, "Hyper"),
        (KeyModifiers::META, "Meta"),
    ]
    .into_iter()
    .filter(|(flag, _)| modifiers.contains(*flag))
    .map(|(_, name)| name.to_string())
    .collect()
}

fn mouse_kind(kind: MouseEventKind) -> (String, Option<String>) {
    match kind {
        MouseEventKind::Down(button) => ("down".into(), Some(mouse_button(button))),
        MouseEventKind::Up(button) => ("up".into(), Some(mouse_button(button))),
        MouseEventKind::Drag(button) => ("drag".into(), Some(mouse_button(button))),
        MouseEventKind::Moved => ("move".into(), None),
        MouseEventKind::ScrollDown => ("scroll_down".into(), None),
        MouseEventKind::ScrollUp => ("scroll_up".into(), None),
        MouseEventKind::ScrollLeft => ("scroll_left".into(), None),
        MouseEventKind::ScrollRight => ("scroll_right".into(), None),
    }
}

fn mouse_button(button: MouseButton) -> String {
    match button {
        MouseButton::Left => "left",
        MouseButton::Right => "right",
        MouseButton::Middle => "middle",
    }
    .into()
}

fn span_style(span: &WireSpan, theme: &crate::theme::Theme) -> Style {
    let mut style = Style::default();
    if let Some(color) = span.fg.as_deref().and_then(|raw| wire_color(raw, theme)) {
        style = style.fg(color);
    }
    if let Some(color) = span.bg.as_deref().and_then(|raw| wire_color(raw, theme)) {
        style = style.bg(color);
    }
    for (enabled, modifier) in [
        (span.bold, Modifier::BOLD),
        (span.dim, Modifier::DIM),
        (span.italic, Modifier::ITALIC),
        (span.underlined, Modifier::UNDERLINED),
        (span.reversed, Modifier::REVERSED),
    ] {
        if enabled {
            style = style.add_modifier(modifier);
        }
    }
    style
}

fn wire_color(raw: &str, theme: &crate::theme::Theme) -> Option<Color> {
    match raw {
        "default" | "reset" => Some(Color::Reset),
        "theme:border" => Some(theme.border),
        "theme:border_focused" => Some(theme.border_focused),
        "theme:rule" => Some(theme.rule),
        "theme:title" => Some(theme.title),
        "theme:text" => Some(theme.text),
        "theme:muted" => Some(theme.muted),
        "theme:label" => Some(theme.label),
        "theme:accent" => Some(theme.accent),
        "theme:key" => Some(theme.key),
        "theme:success" => Some(theme.success),
        "theme:warning" => Some(theme.warning),
        "theme:error" => Some(theme.error),
        "theme:track" => Some(theme.track),
        _ => raw
            .strip_prefix("ansi:")
            .and_then(|index| index.parse::<u8>().ok())
            .map(Color::Indexed)
            .or_else(|| raw.parse().ok()),
    }
}

#[cfg(test)]
mod tests {
    use std::io::{self, BufReader};

    use super::process::{apply_message, read_limited_line};
    use super::*;

    fn detached_panel(policy: InputPolicy) -> PluginPanel {
        PluginPanel {
            spec: PluginConfig {
                id: "test".into(),
                command: vec!["unused".into()],
                config: toml::Table::new(),
            },
            runtime: None,
            shared: Arc::new(Mutex::new(Shared::starting())),
            seen_generation: 0,
            phase: Phase::Running,
            title: "test".into(),
            refresh: DEFAULT_REFRESH,
            frame: None,
            bindings: Vec::new(),
            interrupt_armed: policy.interrupt,
            policy,
            notice: None,
            input_barrier: None,
            last_size: None,
            last_focus: None,
        }
    }

    #[test]
    fn ready_must_match_the_host_protocol() {
        let shared = Arc::new(Mutex::new(Shared::starting()));
        assert!(!apply_message(
            PluginMessage::Ready {
                protocol: PROTOCOL_VERSION + 1,
                title: None,
                refresh_ms: None,
            },
            &shared,
        ));
        assert!(matches!(
            shared.lock().unwrap().phase,
            Phase::Failed(ref message) if message.contains("incompatible")
        ));
    }

    #[test]
    fn frames_are_full_snapshots_and_old_revisions_are_ignored() {
        let shared = Arc::new(Mutex::new(Shared::starting()));
        assert!(apply_message(
            PluginMessage::Ready {
                protocol: PROTOCOL_VERSION,
                title: Some("test".into()),
                refresh_ms: Some(1),
            },
            &shared,
        ));
        let frame = |revision, text: &str| PluginMessage::Frame {
            revision,
            title: None,
            counter: None,
            lines: vec![WireLine {
                spans: vec![WireSpan {
                    text: text.into(),
                    fg: None,
                    bg: None,
                    bold: false,
                    dim: false,
                    italic: false,
                    underlined: false,
                    reversed: false,
                }],
            }],
            bindings: Vec::new(),
            input: InputPolicy::default(),
            cursor: None,
        };
        assert!(apply_message(frame(2, "new"), &shared));
        assert!(apply_message(frame(1, "old"), &shared));
        let shared = shared.lock().unwrap();
        assert_eq!(shared.refresh, Duration::from_millis(16));
        assert_eq!(shared.frame.as_ref().unwrap().lines[0].spans[0].text, "new");
    }

    #[test]
    fn canonical_keys_are_stable_across_platforms() {
        assert_eq!(
            canonical_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            "Ctrl+c"
        );
        assert_eq!(
            canonical_key(KeyEvent::new(KeyCode::PageUp, KeyModifiers::SHIFT)),
            "Shift+PageUp"
        );
        assert_eq!(canonical_key(KeyEvent::from(KeyCode::Enter)), "Enter");
    }

    #[test]
    fn semantic_and_terminal_colours_share_the_wire_format() {
        let theme = crate::theme::Theme::default();
        assert_eq!(wire_color("theme:accent", &theme), Some(theme.accent));
        assert_eq!(wire_color("ansi:203", &theme), Some(Color::Indexed(203)));
        assert_eq!(wire_color("#112233", &theme), Some(Color::Rgb(17, 34, 51)));
    }

    #[test]
    fn passive_policy_claims_only_named_keys_and_capture_never_falls_through() {
        let mut passive = detached_panel(InputPolicy {
            keys: vec!["Enter".into()],
            ..InputPolicy::default()
        });
        assert_eq!(
            passive.handle_key(KeyEvent::from(KeyCode::Enter)),
            KeyOutcome::Consumed
        );
        assert_eq!(
            passive.handle_key(KeyEvent::from(KeyCode::Char('q'))),
            KeyOutcome::Ignored
        );

        let mut capturing = detached_panel(InputPolicy {
            capture: true,
            ..InputPolicy::default()
        });
        assert_eq!(
            capturing.handle_key(KeyEvent::from(KeyCode::Char('q'))),
            KeyOutcome::Consumed,
            "a stalled plugin must not turn its q into a global quit"
        );
    }

    #[test]
    fn a_passive_action_captures_until_a_new_frame_acknowledges_it() {
        let passive = InputPolicy {
            keys: vec!["Enter".into()],
            ..InputPolicy::default()
        };
        let frame = |revision, input| WireFrame {
            revision,
            title: None,
            counter: None,
            lines: Vec::new(),
            bindings: Vec::new(),
            input,
            cursor: None,
        };
        let mut panel = detached_panel(passive.clone());
        panel.frame = Some(frame(4, passive));

        // Simulate a successfully queued named key. Until revision 5 arrives,
        // rapid follow-up input belongs to the pending plugin transition.
        panel.begin_input_barrier();
        assert!(panel.captures_input());
        assert_eq!(
            panel.handle_key(KeyEvent::from(KeyCode::Char('q'))),
            KeyOutcome::Consumed
        );
        assert_eq!(panel.handle_interrupt(), KeyOutcome::Consumed);
        assert_eq!(panel.handle_interrupt(), KeyOutcome::Ignored);

        {
            let mut shared = panel.shared.lock().unwrap();
            shared.frame = Some(frame(5, InputPolicy::default()));
            shared.generation += 1;
        }
        assert!(panel.sync());
        assert!(!panel.captures_input());
        assert_eq!(
            panel.handle_key(KeyEvent::from(KeyCode::Char('q'))),
            KeyOutcome::Ignored
        );
    }

    #[test]
    fn watch_messages_enter_the_native_panel_event_drain() {
        let mut panel = detached_panel(InputPolicy::default());
        assert!(apply_message(
            PluginMessage::Ready {
                protocol: PROTOCOL_VERSION,
                title: None,
                refresh_ms: None,
            },
            &panel.shared,
        ));
        assert!(apply_message(
            PluginMessage::Watch {
                text: "the build finished".into(),
            },
            &panel.shared,
        ));

        let events = panel.events();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].source, "test");
        assert_eq!(events[0].text, "the build finished");
        assert!(panel.events().is_empty(), "the native hook is a drain");
    }

    #[test]
    fn watch_messages_are_single_lines_and_their_queue_is_bounded() {
        let shared = Arc::new(Mutex::new(Shared::starting()));
        assert!(apply_message(
            PluginMessage::Ready {
                protocol: PROTOCOL_VERSION,
                title: None,
                refresh_ms: None,
            },
            &shared,
        ));
        for index in 0..65 {
            assert!(apply_message(
                PluginMessage::Watch {
                    text: format!("event {index}"),
                },
                &shared,
            ));
        }
        let guard = shared.lock().unwrap();
        assert_eq!(guard.watch.len(), 64);
        assert_eq!(guard.watch.front().map(String::as_str), Some("event 1"));
        drop(guard);

        assert!(!apply_message(
            PluginMessage::Watch {
                text: "not\none line".into(),
            },
            &shared,
        ));
        assert!(matches!(
            shared.lock().unwrap().phase,
            Phase::Failed(ref error) if error.contains("one non-empty line")
        ));
    }

    #[test]
    fn interrupt_policy_is_locally_one_shot() {
        let mut panel = detached_panel(InputPolicy {
            capture: true,
            interrupt: true,
            ..InputPolicy::default()
        });
        assert_eq!(panel.handle_interrupt(), KeyOutcome::Consumed);
        assert_eq!(panel.handle_interrupt(), KeyOutcome::Ignored);

        let _ = panel.handle_key(KeyEvent::from(KeyCode::Char('x')));
        assert_eq!(panel.handle_interrupt(), KeyOutcome::Consumed);
    }

    #[test]
    fn oversized_messages_are_rejected_before_unbounded_allocation() {
        let input = vec![b'x'; MAX_MESSAGE_BYTES + 1];
        let mut reader = BufReader::new(input.as_slice());
        let error = read_limited_line(&mut reader, &mut Vec::new()).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }

    /// Cross-repository smoke test. Point `MIRADOR_TEST_PLUGIN` at any protocol
    /// command; the Python terminal package uses this in local release checks.
    #[test]
    #[ignore = "requires an explicitly installed external plugin command"]
    fn an_external_process_negotiates_and_publishes_a_frame() {
        use std::time::Instant;

        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let command = std::env::var("MIRADOR_TEST_PLUGIN")
            .expect("set MIRADOR_TEST_PLUGIN to an external plugin executable");
        let mut panel = PluginPanel::new(PluginConfig {
            id: "integration".into(),
            command: vec![command],
            config: toml::Table::new(),
        });
        let theme = crate::theme::Theme::default();
        let gradients = theme.gradients();
        let watch = crate::watch::WatchLog::default();
        let mut terminal = Terminal::new(TestBackend::new(60, 12)).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);

        while Instant::now() < deadline {
            panel.tick();
            terminal
                .draw(|frame| {
                    let area = frame.area();
                    panel.render(
                        frame,
                        area,
                        RenderContext {
                            theme: &theme,
                            gradients: &gradients,
                            focused: true,
                            watch: &watch,
                        },
                    );
                })
                .unwrap();
            if matches!(panel.phase, Phase::Running) && panel.frame.is_some() {
                panel.shutdown();
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }

        let phase = panel.phase.clone();
        panel.shutdown();
        panic!("plugin did not publish a frame within five seconds: {phase:?}");
    }

    /// Full proof for the dashboard-native example: Rust host -> Python SDK ->
    /// process watcher -> child process -> protocol watch message -> native
    /// `Panel::events` drain.
    #[test]
    #[ignore = "requires an explicitly installed mirador-process-watch command"]
    fn an_external_process_watcher_reports_a_native_watch_event() {
        use std::time::Instant;

        let watcher = std::env::var("MIRADOR_TEST_WATCH_PLUGIN")
            .expect("set MIRADOR_TEST_WATCH_PLUGIN to mirador-process-watch");
        let child = std::env::current_exe()
            .expect("test executable has a path")
            .to_string_lossy()
            .into_owned();
        let mut config = toml::Table::new();
        config.insert("name".into(), toml::Value::String("Host tests".into()));
        config.insert(
            "command".into(),
            toml::Value::Array(
                [child, "--list".into()]
                    .into_iter()
                    .map(toml::Value::String)
                    .collect(),
            ),
        );
        config.insert("autostart".into(), toml::Value::Boolean(true));
        config.insert("output_lines".into(), toml::Value::Integer(50));
        let mut panel = PluginPanel::new(PluginConfig {
            id: "process-watch-test".into(),
            command: vec![watcher],
            config,
        });
        let deadline = Instant::now() + Duration::from_secs(15);

        while Instant::now() < deadline {
            panel.tick();
            if let Some(event) = panel.events().into_iter().next() {
                panel.shutdown();
                assert_eq!(event.source, "process-watch-test");
                assert!(
                    event.text.contains("finished successfully"),
                    "unexpected Watch Log event: {}",
                    event.text
                );
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }

        let phase = panel.phase.clone();
        panel.shutdown();
        panic!("process watcher did not report completion within fifteen seconds: {phase:?}");
    }
}
