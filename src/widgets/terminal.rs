//! An interactive shell inside a dashboard panel.
//!
//! A nested terminal is two different jobs. [`portable_pty`] owns the real
//! platform terminal (a Unix PTY or Windows `ConPTY`), while [`tui_term`] renders
//! a parsed VT screen into ratatui's buffer. Neither job happens on mirador's
//! event thread: one worker reads and parses output, one writes input and
//! applies resizes, and one waits for the child. The panel only swaps in the
//! newest bounded, visible-screen snapshot from [`Panel::tick`]. A command
//! producing megabytes can therefore make its own worker busy without making
//! the dashboard stop accepting keys.
//!
//! There is deliberately no second renderer. Ratatui owns the outer terminal
//! and diffing two independently drawn buffers would corrupt both of them.
//! The background side publishes state; mirador's one render loop remains the
//! only code that writes to the real screen.
//!
//! # Input and the way out
//!
//! Focus alone is not enough to hand every key to a shell: `Tab`, `q` and `?`
//! are how someone moves around and leaves the dashboard. `Enter` therefore
//! engages the terminal and `Ctrl+G` releases it. While engaged the panel's
//! [`Panel::captures_input`] veto is active and every other key belongs to the
//! child.
//!
//! `Ctrl+C` is the unavoidable exception to that exception. A shell needs it
//! to interrupt a foreground command, and mirador promises an unconditional
//! escape from every state. The first press is sent to the PTY and disarms the
//! panel; a second consecutive press falls through to the application and
//! quits. Any other input re-arms it. The useful rule stays simple: Ctrl+C
//! twice always quits.

use std::ffi::OsString;
use std::io::{ErrorKind, Read, Write};
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::{Arc, Mutex, MutexGuard, TryLockError};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result};
use portable_pty::{ChildKiller, CommandBuilder, PtySize};
use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseEvent, MouseEventKind};
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::widgets::Paragraph;
use tui_term::vt100;
use tui_term::widget::{Cursor, PseudoTerminal, Screen as TerminalScreen};

use crate::config::TerminalConfig;
use crate::frame::Binding;
use crate::panel::{KeyOutcome, Panel, RenderContext};

const DEFAULT_COLS: u16 = 80;
const DEFAULT_ROWS: u16 = 24;
const SCROLL_STEP: i32 = 3;

const PASSIVE_BINDINGS: &[Binding] = &[
    Binding::primary("↵", "use terminal"),
    Binding::extra("Shift+PgUp / PgDn", "scroll output"),
    Binding::extra("r", "restart an exited shell"),
];

const ACTIVE_BINDINGS: &[Binding] = &[
    Binding::primary("Ctrl+G", "dashboard"),
    Binding::primary("Ctrl+C", "interrupt; twice quits"),
    Binding::extra("Shift+PgUp / PgDn", "scroll output"),
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TerminalSize {
    cols: u16,
    rows: u16,
}

impl TerminalSize {
    fn new(cols: u16, rows: u16) -> Self {
        Self {
            cols: cols.max(1),
            rows: rows.max(1),
        }
    }

    const fn as_pty(self) -> PtySize {
        PtySize {
            rows: self.rows,
            cols: self.cols,
            pixel_width: 0,
            pixel_height: 0,
        }
    }
}

impl Default for TerminalSize {
    fn default() -> Self {
        Self::new(DEFAULT_COLS, DEFAULT_ROWS)
    }
}

/// Just the cells a frame can draw, detached from the parser and its full
/// scrollback buffer.
///
/// Capturing this on the reader thread is the boundary that matters. Cloning a
/// `vt100::Screen` in `tick` would also clone every retained scrollback row on
/// the UI thread; this stays proportional to the visible panel instead.
struct ScreenSnapshot {
    rows: u16,
    cols: u16,
    cells: Vec<Option<vt100::Cell>>,
    cursor: (u16, u16),
    hide_cursor: bool,
    application_cursor: bool,
    bracketed_paste: bool,
    scrollback: usize,
}

impl ScreenSnapshot {
    fn capture(screen: &vt100::Screen) -> Self {
        let (rows, cols) = screen.size();
        let mut cells = Vec::with_capacity(usize::from(rows) * usize::from(cols));
        for row in 0..rows {
            for col in 0..cols {
                cells.push(screen.cell(row, col).cloned());
            }
        }

        let (cursor_row, cursor_col) = screen.cursor_position();
        let offset = u16::try_from(screen.scrollback()).unwrap_or(u16::MAX);
        Self {
            rows,
            cols,
            cells,
            cursor: (cursor_row.saturating_add(offset), cursor_col),
            hide_cursor: screen.hide_cursor(),
            application_cursor: screen.application_cursor(),
            bracketed_paste: screen.bracketed_paste(),
            scrollback: screen.scrollback(),
        }
    }
}

impl TerminalScreen for ScreenSnapshot {
    type C = vt100::Cell;

    fn cell(&self, row: u16, col: u16) -> Option<&Self::C> {
        if row >= self.rows || col >= self.cols {
            return None;
        }
        let index = usize::from(row) * usize::from(self.cols) + usize::from(col);
        self.cells.get(index).and_then(Option::as_ref)
    }

    fn hide_cursor(&self) -> bool {
        self.hide_cursor
    }

    fn cursor_position(&self) -> (u16, u16) {
        self.cursor
    }
}

#[derive(Debug, Clone)]
struct LaunchSpec {
    command: Vec<String>,
    cwd: PathBuf,
    scrollback: usize,
}

impl LaunchSpec {
    fn new(config: &TerminalConfig) -> Result<Self> {
        Ok(Self {
            command: config.command.clone(),
            cwd: std::env::current_dir().context("finding the terminal's starting directory")?,
            scrollback: config.scrollback,
        })
    }

    fn command(&self) -> CommandBuilder {
        let mut command = if self.command.is_empty() {
            CommandBuilder::new_default_prog()
        } else {
            let argv = self.command.iter().map(OsString::from).collect();
            CommandBuilder::from_argv(argv)
        };
        command.cwd(&self.cwd);
        // This is the terminal model the parser implements, not an attempt to
        // impersonate whichever application hosts mirador.
        command.env("TERM", "xterm-256color");
        command.env("COLORTERM", "truecolor");
        command
    }
}

enum Control {
    Input(Vec<u8>),
    Reply(Vec<u8>),
    Resize(TerminalSize),
    Scroll(i32),
    Shutdown,
}

#[derive(Clone)]
struct TerminalCallbacks {
    replies: Sender<Control>,
}

impl vt100::Callbacks for TerminalCallbacks {
    fn unhandled_csi(
        &mut self,
        screen: &mut vt100::Screen,
        intermediate: Option<u8>,
        _second_intermediate: Option<u8>,
        params: &[&[u16]],
        final_character: char,
    ) {
        let first = params.first().and_then(|param| param.first()).copied();
        let response = match (intermediate, first, final_character) {
            // Device status and cursor-position reports. Windows ConPTY asks
            // for the latter before it will finish bringing up an interactive
            // shell, so treating terminal output as write-only deadlocks cmd.
            (None, Some(5), 'n') => Some(b"\x1b[0n".to_vec()),
            (None, Some(6), 'n') => {
                let (row, col) = screen.cursor_position();
                Some(format!("\x1b[{};{}R", row + 1, col + 1).into_bytes())
            }
            (Some(b'?'), Some(6), 'n') => {
                let (row, col) = screen.cursor_position();
                Some(format!("\x1b[?{};{}R", row + 1, col + 1).into_bytes())
            }
            // A conservative VT100-with-advanced-video identity. Applications
            // use this to choose escape sequences, not to identify mirador.
            (None, None | Some(0), 'c') => Some(b"\x1b[?1;2c".to_vec()),
            (Some(b'>'), None | Some(0), 'c') => Some(b"\x1b[>0;100;0c".to_vec()),
            // Report the character-cell size for xterm's window-size query.
            (None, Some(18), 't') => {
                let (rows, cols) = screen.size();
                Some(format!("\x1b[8;{rows};{cols}t").into_bytes())
            }
            _ => None,
        };
        if let Some(response) = response {
            let _ = self.replies.send(Control::Reply(response));
        }
    }
}

type TerminalParser = vt100::Parser<TerminalCallbacks>;

enum SessionEvent {
    Exited { code: u32, signal: Option<String> },
    Failed(String),
}

struct Session {
    control: Sender<Control>,
    latest: Arc<Mutex<Option<ScreenSnapshot>>>,
    events: Receiver<SessionEvent>,
    killer: Option<Box<dyn ChildKiller + Send + Sync>>,
}

impl Session {
    fn start(spec: &LaunchSpec, size: TerminalSize) -> Result<Self> {
        let pty = portable_pty::native_pty_system();
        let pair = pty
            .openpty(size.as_pty())
            .context("opening a pseudoterminal for `[terminal]`")?;
        let portable_pty::PtyPair { master, slave } = pair;

        let mut child = slave
            .spawn_command(spec.command())
            .with_context(|| describe_start_error(&spec.command))?;
        drop(slave);
        let reader = master
            .try_clone_reader()
            .context("opening the terminal output stream")?;
        let writer = master
            .take_writer()
            .context("opening the terminal input stream")?;

        let killer = child.clone_killer();
        let mut emergency_killer = child.clone_killer();
        let (event_tx, events) = mpsc::channel();
        let (control, control_rx) = mpsc::channel();
        let parser = Arc::new(Mutex::new(vt100::Parser::new_with_callbacks(
            size.rows,
            size.cols,
            spec.scrollback,
            TerminalCallbacks {
                replies: control.clone(),
            },
        )));
        let latest = Arc::new(Mutex::new(None));
        {
            let parser = lock(&parser);
            publish(&parser, &latest);
        }

        // Move the child into its waiter first. If either I/O worker fails to
        // spawn below, killing the child releases this waiter and every local
        // handle can unwind. Starting the input worker first would create a
        // rare cycle on a later spawn failure: its parser owns the command
        // sender while that worker owns the matching receiver.
        let child_events = event_tx.clone();
        if let Err(error) = thread::Builder::new()
            .name("mirador-terminal-child".into())
            .spawn(move || match child.wait() {
                Ok(status) => {
                    let _ = child_events.send(SessionEvent::Exited {
                        code: status.exit_code(),
                        signal: status.signal().map(str::to_string),
                    });
                }
                Err(error) => {
                    let _ = child_events.send(SessionEvent::Failed(format!(
                        "waiting for the terminal shell failed: {error}"
                    )));
                }
            })
        {
            let _ = emergency_killer.kill();
            return Err(error).context("starting the terminal child waiter");
        }

        let output_parser = Arc::clone(&parser);
        let output_latest = Arc::clone(&latest);
        let output_events = event_tx.clone();
        if let Err(error) = thread::Builder::new()
            .name("mirador-terminal-output".into())
            .spawn(move || output_worker(reader, &output_parser, &output_latest, &output_events))
        {
            let _ = emergency_killer.kill();
            return Err(error).context("starting the terminal output worker");
        }

        let control_parser = Arc::clone(&parser);
        let control_latest = Arc::clone(&latest);
        let control_events = event_tx.clone();
        if let Err(error) = thread::Builder::new()
            .name("mirador-terminal-input".into())
            .spawn(move || {
                control_worker(
                    control_rx,
                    writer,
                    master,
                    &control_parser,
                    &control_latest,
                    &control_events,
                );
            })
        {
            let _ = emergency_killer.kill();
            return Err(error).context("starting the terminal input worker");
        }

        Ok(Self {
            control,
            latest,
            events,
            killer: Some(killer),
        })
    }

    fn send(&self, control: Control) -> bool {
        self.control.send(control).is_ok()
    }

    fn take_snapshot(&self) -> Option<ScreenSnapshot> {
        match self.latest.try_lock() {
            Ok(mut latest) => latest.take(),
            Err(TryLockError::Poisoned(poisoned)) => poisoned.into_inner().take(),
            Err(TryLockError::WouldBlock) => None,
        }
    }

    fn shutdown(&mut self) {
        let _ = self.control.send(Control::Shutdown);
        if let Some(mut killer) = self.killer.take() {
            // `kill` is the only reliable way to release a child whose stdin
            // is full and has left the input worker blocked in `write`.
            let _ = killer.kill();
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn describe_start_error(command: &[String]) -> String {
    if let Some(program) = command.first() {
        format!(
            "starting `[terminal].command` program `{program}`; check that it is installed and on PATH"
        )
    } else {
        "starting the platform's default shell; set `[terminal].command` to an installed program"
            .to_string()
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn publish(parser: &TerminalParser, latest: &Mutex<Option<ScreenSnapshot>>) {
    *lock(latest) = Some(ScreenSnapshot::capture(parser.screen()));
}

fn output_worker(
    mut reader: Box<dyn Read + Send>,
    parser: &Mutex<TerminalParser>,
    latest: &Mutex<Option<ScreenSnapshot>>,
    events: &Sender<SessionEvent>,
) {
    let mut bytes = [0_u8; 8_192];
    loop {
        match reader.read(&mut bytes) {
            Ok(0) => return,
            Ok(read) => {
                let mut parser = lock(parser);
                parser.process(&bytes[..read]);
                publish(&parser, latest);
            }
            Err(error) if error.kind() == ErrorKind::Interrupted => {}
            // Closing a ConPTY reports a broken pipe rather than EOF. The
            // child waiter owns the useful exit status, so this is not a
            // second failure to put in front of the reader.
            Err(error) if error.kind() == ErrorKind::BrokenPipe => return,
            Err(error) => {
                let _ = events.send(SessionEvent::Failed(format!(
                    "reading terminal output failed: {error}"
                )));
                return;
            }
        }
    }
}

fn control_worker(
    controls: Receiver<Control>,
    mut writer: Box<dyn Write + Send>,
    master: Box<dyn portable_pty::MasterPty + Send>,
    parser: &Mutex<TerminalParser>,
    latest: &Mutex<Option<ScreenSnapshot>>,
    events: &Sender<SessionEvent>,
) {
    while let Ok(control) = controls.recv() {
        match control {
            Control::Input(bytes) => {
                // User input, especially Ctrl+C, takes precedence over moving
                // the viewport. A noisy child may keep the parser busy, but it
                // must not delay delivery of the key intended to stop it.
                if let Err(error) = writer.write_all(&bytes).and_then(|()| writer.flush()) {
                    let _ = events.send(SessionEvent::Failed(format!(
                        "writing terminal input failed: {error}"
                    )));
                    return;
                }
                {
                    let mut parser = lock(parser);
                    if parser.screen().scrollback() != 0 {
                        parser.screen_mut().set_scrollback(0);
                        publish(&parser, latest);
                    }
                }
            }
            Control::Reply(bytes) => {
                if let Err(error) = writer.write_all(&bytes).and_then(|()| writer.flush()) {
                    let _ = events.send(SessionEvent::Failed(format!(
                        "answering a terminal query failed: {error}"
                    )));
                    return;
                }
            }
            Control::Resize(size) => {
                if let Err(error) = master.resize(size.as_pty()) {
                    let _ = events.send(SessionEvent::Failed(format!(
                        "resizing the terminal failed: {error:#}"
                    )));
                    return;
                }
                let mut parser = lock(parser);
                if parser.screen().size() != (size.rows, size.cols) {
                    parser.screen_mut().set_size(size.rows, size.cols);
                    publish(&parser, latest);
                }
            }
            Control::Scroll(delta) => {
                let mut parser = lock(parser);
                let current = parser.screen().scrollback();
                let distance = delta.unsigned_abs() as usize;
                let requested = if delta >= 0 {
                    current.saturating_add(distance)
                } else {
                    current.saturating_sub(distance)
                };
                parser.screen_mut().set_scrollback(requested);
                publish(&parser, latest);
            }
            Control::Shutdown => return,
        }
    }
    // These handles belong to this worker. Consuming them here makes the
    // lifetime explicit: disconnecting the command channel closes the PTY.
    drop(master);
    drop(controls);
}

enum Backend {
    Starting(Receiver<Result<Session, String>>),
    Running(Session),
    Empty,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Phase {
    Starting,
    Running,
    Exited(String),
    Failed(String),
}

/// A persistent interactive shell rendered inside one mirador panel.
pub struct TerminalPanel {
    spec: LaunchSpec,
    backend: Backend,
    phase: Phase,
    snapshot: Option<ScreenSnapshot>,
    active: bool,
    interrupt_ready: bool,
    size: TerminalSize,
}

impl TerminalPanel {
    pub fn new(config: &TerminalConfig) -> Result<Self> {
        let spec = LaunchSpec::new(config)?;
        let size = TerminalSize::default();
        let backend = Backend::Starting(spawn_session(spec.clone(), size)?);
        Ok(Self {
            spec,
            backend,
            phase: Phase::Starting,
            snapshot: None,
            active: false,
            interrupt_ready: true,
            size,
        })
    }

    fn restart(&mut self) {
        self.stop_backend();
        self.snapshot = None;
        self.active = false;
        self.interrupt_ready = true;
        match spawn_session(self.spec.clone(), self.size) {
            Ok(starting) => {
                self.backend = Backend::Starting(starting);
                self.phase = Phase::Starting;
            }
            Err(error) => {
                self.phase =
                    Phase::Failed(format!("starting the terminal worker failed: {error:#}"));
            }
        }
    }

    fn stop_backend(&mut self) {
        let mut backend = std::mem::replace(&mut self.backend, Backend::Empty);
        if let Backend::Running(session) = &mut backend {
            session.shutdown();
        }
    }

    fn send(&mut self, control: Control) -> bool {
        let sent = matches!(&self.backend, Backend::Running(session) if session.send(control));
        if !sent {
            self.active = false;
            if matches!(self.phase, Phase::Running) {
                self.phase = Phase::Failed(
                    "the terminal worker stopped; press r to start a new shell".to_string(),
                );
            }
        }
        sent
    }

    fn scroll(&mut self, delta: i32) -> KeyOutcome {
        if self.send(Control::Scroll(delta)) {
            KeyOutcome::Consumed
        } else {
            KeyOutcome::Ignored
        }
    }

    fn page(&self) -> i32 {
        i32::from(self.size.rows.saturating_sub(1).max(1))
    }

    fn is_running(&self) -> bool {
        matches!(self.phase, Phase::Running) && matches!(self.backend, Backend::Running(_))
    }

    fn update_size(&mut self, area: Rect) {
        let size = TerminalSize::new(area.width, area.height);
        if size != self.size {
            self.size = size;
            let _ = self.send(Control::Resize(size));
        }
    }

    fn phase_message(&self) -> String {
        match &self.phase {
            Phase::Starting => "Starting the shell…".to_string(),
            Phase::Running => "Waiting for the shell…".to_string(),
            Phase::Exited(status) => format!("Shell exited ({status}). Press r to restart it."),
            Phase::Failed(error) => format!("{error}\n\nPress r to start a new shell."),
        }
    }
}

impl Drop for TerminalPanel {
    fn drop(&mut self) {
        self.stop_backend();
    }
}

impl Panel for TerminalPanel {
    fn title(&self) -> String {
        "Terminal".to_string()
    }

    fn counter(&self) -> Option<String> {
        if self.active {
            return Some("input".to_string());
        }
        if let Some(snapshot) = &self.snapshot
            && snapshot.scrollback > 0
        {
            return Some(format!("{} up", snapshot.scrollback));
        }
        match self.phase {
            Phase::Starting => Some("starting".to_string()),
            Phase::Exited(_) => Some("exited".to_string()),
            Phase::Failed(_) => Some("error".to_string()),
            Phase::Running => None,
        }
    }

    fn bindings(&self) -> &'static [Binding] {
        if self.active {
            ACTIVE_BINDINGS
        } else {
            PASSIVE_BINDINGS
        }
    }

    fn refresh_interval(&self) -> Duration {
        // While this panel captures input, the app honours this as a shorter
        // event wait so asynchronous shell echo feels immediate. Once released,
        // the configured dashboard cadence takes over again; even while active,
        // no snapshot still means no redraw.
        Duration::from_millis(16)
    }

    fn tick(&mut self) -> bool {
        let launch = match &self.backend {
            Backend::Starting(receiver) => match receiver.try_recv() {
                Ok(result) => Some(result),
                Err(TryRecvError::Empty) => None,
                Err(TryRecvError::Disconnected) => Some(Err(
                    "the terminal launch worker stopped before returning a session".to_string(),
                )),
            },
            Backend::Running(_) | Backend::Empty => None,
        };

        let mut changed = false;
        if let Some(launch) = launch {
            match launch {
                Ok(session) => {
                    self.backend = Backend::Running(session);
                    self.phase = Phase::Running;
                    // The first frame is commonly drawn while the launch
                    // worker is still opening the PTY. Its resize cannot be
                    // delivered yet, so apply the latest desired size now
                    // rather than leaving the child at the 80x24 bootstrap.
                    let _ = self.send(Control::Resize(self.size));
                }
                Err(error) => {
                    self.backend = Backend::Empty;
                    self.phase = Phase::Failed(error);
                    self.active = false;
                }
            }
            changed = true;
        }

        let (snapshot, events) = match &self.backend {
            Backend::Running(session) => (
                session.take_snapshot(),
                session.events.try_iter().collect::<Vec<_>>(),
            ),
            Backend::Starting(_) | Backend::Empty => (None, Vec::new()),
        };

        if let Some(snapshot) = snapshot {
            self.snapshot = Some(snapshot);
            changed = true;
        }

        let mut failed = false;
        for event in events {
            match event {
                SessionEvent::Exited { code, signal } => {
                    if !matches!(self.phase, Phase::Failed(_)) {
                        let status = signal.unwrap_or_else(|| format!("code {code}"));
                        self.phase = Phase::Exited(status);
                    }
                    self.active = false;
                }
                SessionEvent::Failed(error) => {
                    // A normal child exit often closes its pipes first. The
                    // workers suppress those expected closure errors; anything
                    // that reaches here is the useful, actionable failure.
                    self.phase = Phase::Failed(error);
                    self.active = false;
                    failed = true;
                }
            }
            changed = true;
        }
        if failed && let Backend::Running(session) = &mut self.backend {
            // Once either I/O path is gone the PTY cannot recover in place.
            // Do not leave an invisible shell (or its foreground command)
            // running behind an error panel while waiting for an explicit r.
            session.shutdown();
        }

        changed
    }

    fn render(&mut self, frame: &mut Frame, area: Rect, ctx: RenderContext<'_>) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        self.update_size(area);

        if matches!(self.phase, Phase::Failed(_))
            || (self.snapshot.is_none() && !matches!(self.phase, Phase::Running))
        {
            let style = Style::default().fg(if matches!(self.phase, Phase::Failed(_)) {
                ctx.theme.error
            } else {
                ctx.theme.muted
            });
            frame.render_widget(
                Paragraph::new(crate::grid::wrapped(&self.phase_message(), area.width))
                    .style(style),
                area,
            );
            return;
        }

        if let Some(snapshot) = &self.snapshot {
            let cursor = Cursor::default().visibility(
                self.active && ctx.focused && !snapshot.hide_cursor && snapshot.scrollback == 0,
            );
            frame.render_widget(PseudoTerminal::new(snapshot).cursor(cursor), area);
        } else {
            frame.render_widget(
                Paragraph::new(self.phase_message()).style(Style::default().fg(ctx.theme.muted)),
                area,
            );
        }
    }

    fn handle_key(&mut self, key: KeyEvent) -> KeyOutcome {
        // `handle_interrupt` receives Ctrl+C before ordinary key dispatch.
        // Reaching this method therefore proves another key intervened, which
        // re-arms the child interrupt even when that key returns early below.
        if self.active {
            self.interrupt_ready = true;
        }

        let shifted = key.modifiers.contains(KeyModifiers::SHIFT);
        if shifted {
            match key.code {
                KeyCode::PageUp => return self.scroll(self.page()),
                KeyCode::PageDown => return self.scroll(-self.page()),
                _ => {}
            }
        }

        if !self.active {
            match key.code {
                KeyCode::Enter if self.is_running() => {
                    self.active = true;
                    self.interrupt_ready = true;
                    let _ = self.send(Control::Scroll(-i32::MAX));
                    return KeyOutcome::Consumed;
                }
                KeyCode::Char('r') if matches!(self.phase, Phase::Exited(_) | Phase::Failed(_)) => {
                    self.restart();
                    return KeyOutcome::Consumed;
                }
                _ => return KeyOutcome::Ignored,
            }
        }

        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if ctrl && matches!(key.code, KeyCode::Char('g' | 'G')) {
            self.active = false;
            self.interrupt_ready = true;
            return KeyOutcome::Consumed;
        }

        let application_cursor = self
            .snapshot
            .as_ref()
            .is_some_and(|snapshot| snapshot.application_cursor);
        if let Some(bytes) = encode_key(key, application_cursor) {
            let _ = self.send(Control::Input(bytes));
        }
        // Unknown media and lock keys still belong to the engaged terminal;
        // letting one fall through could turn a new crossterm key into a future
        // global action without the terminal ever having opted into it.
        KeyOutcome::Consumed
    }

    fn handle_interrupt(&mut self) -> KeyOutcome {
        if !self.active || !self.is_running() || !self.interrupt_ready {
            return KeyOutcome::Ignored;
        }
        if self.send(Control::Input(vec![0x03])) {
            self.interrupt_ready = false;
            KeyOutcome::Consumed
        } else {
            KeyOutcome::Ignored
        }
    }

    fn handle_paste(&mut self, text: &str) -> KeyOutcome {
        if !self.active || !self.is_running() {
            return KeyOutcome::Ignored;
        }

        self.interrupt_ready = true;
        let bracketed = self
            .snapshot
            .as_ref()
            .is_some_and(|snapshot| snapshot.bracketed_paste);
        let bytes = paste_bytes(text, bracketed);
        let _ = self.send(Control::Input(bytes));
        KeyOutcome::Consumed
    }

    fn handle_mouse(&mut self, event: MouseEvent, _area: Rect) -> KeyOutcome {
        match event.kind {
            MouseEventKind::ScrollUp => {
                self.interrupt_ready = true;
                self.scroll(SCROLL_STEP)
            }
            MouseEventKind::ScrollDown => {
                self.interrupt_ready = true;
                self.scroll(-SCROLL_STEP)
            }
            _ => KeyOutcome::Ignored,
        }
    }

    fn captures_input(&self) -> bool {
        self.active
    }

    fn shutdown(&mut self) {
        self.stop_backend();
    }
}

fn paste_bytes(text: &str, bracketed: bool) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(text.len() + if bracketed { 12 } else { 0 });
    if bracketed {
        bytes.extend_from_slice(b"\x1b[200~");
    }
    bytes.extend_from_slice(text.as_bytes());
    if bracketed {
        bytes.extend_from_slice(b"\x1b[201~");
    }
    bytes
}

fn spawn_session(
    spec: LaunchSpec,
    size: TerminalSize,
) -> Result<Receiver<Result<Session, String>>> {
    let (sender, receiver) = mpsc::sync_channel(1);
    thread::Builder::new()
        .name("mirador-terminal-launch".into())
        .spawn(move || {
            let result = Session::start(&spec, size).map_err(|error| format!("{error:#}"));
            let _ = sender.send(result);
        })
        .context("starting the terminal launch worker")?;
    Ok(receiver)
}

fn encode_key(key: KeyEvent, application_cursor: bool) -> Option<Vec<u8>> {
    let modifiers = key.modifiers;
    match key.code {
        KeyCode::Char(character) => Some(encode_character(character, modifiers)),
        KeyCode::Enter => Some(with_alt(enter_sequence(), modifiers)),
        KeyCode::Backspace => Some(with_alt(vec![0x7f], modifiers)),
        KeyCode::Tab if modifiers.contains(KeyModifiers::SHIFT) => Some(b"\x1b[Z".to_vec()),
        KeyCode::Tab => Some(with_alt(vec![b'\t'], modifiers)),
        KeyCode::BackTab => Some(b"\x1b[Z".to_vec()),
        KeyCode::Esc => Some(vec![0x1b]),
        KeyCode::Left => Some(cursor_sequence(b'D', application_cursor, modifiers)),
        KeyCode::Right => Some(cursor_sequence(b'C', application_cursor, modifiers)),
        KeyCode::Up => Some(cursor_sequence(b'A', application_cursor, modifiers)),
        KeyCode::Down => Some(cursor_sequence(b'B', application_cursor, modifiers)),
        KeyCode::Home => Some(cursor_sequence(b'H', application_cursor, modifiers)),
        KeyCode::End => Some(cursor_sequence(b'F', application_cursor, modifiers)),
        KeyCode::Insert => Some(tilde_sequence(2, modifiers)),
        KeyCode::Delete => Some(tilde_sequence(3, modifiers)),
        KeyCode::PageUp => Some(tilde_sequence(5, modifiers)),
        KeyCode::PageDown => Some(tilde_sequence(6, modifiers)),
        KeyCode::F(number) => function_sequence(number, modifiers),
        KeyCode::Null => Some(vec![0]),
        KeyCode::CapsLock
        | KeyCode::ScrollLock
        | KeyCode::NumLock
        | KeyCode::PrintScreen
        | KeyCode::Pause
        | KeyCode::Menu
        | KeyCode::KeypadBegin
        | KeyCode::Media(_)
        | KeyCode::Modifier(_) => None,
    }
}

#[cfg(windows)]
fn enter_sequence() -> Vec<u8> {
    vec![b'\r', b'\n']
}

#[cfg(not(windows))]
fn enter_sequence() -> Vec<u8> {
    vec![b'\n']
}

fn encode_character(character: char, modifiers: KeyModifiers) -> Vec<u8> {
    let mut bytes = if modifiers.contains(KeyModifiers::CONTROL) {
        control_byte(character).map_or_else(
            || character.to_string().into_bytes(),
            |control| vec![control],
        )
    } else {
        character.to_string().into_bytes()
    };
    if modifiers.contains(KeyModifiers::ALT) {
        bytes.insert(0, 0x1b);
    }
    bytes
}

fn control_byte(character: char) -> Option<u8> {
    match character.to_ascii_uppercase() {
        '@' | ' ' | '2' => Some(0x00),
        c @ 'A'..='Z' => Some(c as u8 - b'@'),
        '[' | '3' => Some(0x1b),
        '\\' | '4' => Some(0x1c),
        ']' | '5' => Some(0x1d),
        '^' | '6' => Some(0x1e),
        '_' | '-' | '7' => Some(0x1f),
        '?' | '8' => Some(0x7f),
        _ => None,
    }
}

fn with_alt(mut bytes: Vec<u8>, modifiers: KeyModifiers) -> Vec<u8> {
    if modifiers.contains(KeyModifiers::ALT) {
        bytes.insert(0, 0x1b);
    }
    bytes
}

fn terminal_modifier(modifiers: KeyModifiers) -> u8 {
    1 + u8::from(modifiers.contains(KeyModifiers::SHIFT))
        + 2 * u8::from(modifiers.contains(KeyModifiers::ALT))
        + 4 * u8::from(modifiers.contains(KeyModifiers::CONTROL))
}

fn has_terminal_modifier(modifiers: KeyModifiers) -> bool {
    modifiers.intersects(KeyModifiers::SHIFT | KeyModifiers::ALT | KeyModifiers::CONTROL)
}

fn cursor_sequence(final_byte: u8, application: bool, modifiers: KeyModifiers) -> Vec<u8> {
    if has_terminal_modifier(modifiers) {
        return format!(
            "\x1b[1;{}{}",
            terminal_modifier(modifiers),
            char::from(final_byte)
        )
        .into_bytes();
    }
    vec![0x1b, if application { b'O' } else { b'[' }, final_byte]
}

fn tilde_sequence(number: u8, modifiers: KeyModifiers) -> Vec<u8> {
    if has_terminal_modifier(modifiers) {
        format!("\x1b[{number};{}~", terminal_modifier(modifiers)).into_bytes()
    } else {
        format!("\x1b[{number}~").into_bytes()
    }
}

fn function_sequence(number: u8, modifiers: KeyModifiers) -> Option<Vec<u8>> {
    let final_byte = match number {
        1 => Some('P'),
        2 => Some('Q'),
        3 => Some('R'),
        4 => Some('S'),
        _ => None,
    };
    if let Some(final_byte) = final_byte {
        return Some(if has_terminal_modifier(modifiers) {
            format!("\x1b[1;{}{final_byte}", terminal_modifier(modifiers)).into_bytes()
        } else {
            format!("\x1bO{final_byte}").into_bytes()
        });
    }

    let code = match number {
        5 => 15,
        6 => 17,
        7 => 18,
        8 => 19,
        9 => 20,
        10 => 21,
        11 => 23,
        12 => 24,
        _ => return None,
    };
    Some(tilde_sequence(code, modifiers))
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::*;

    fn key(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, modifiers)
    }

    fn engaged_panel() -> (TerminalPanel, Receiver<Control>) {
        let config = TerminalConfig::default();
        let (control, controls) = mpsc::channel();
        let (_event_sender, events) = mpsc::channel();
        let panel = TerminalPanel {
            spec: LaunchSpec::new(&config).expect("the test process has a working directory"),
            backend: Backend::Running(Session {
                control,
                latest: Arc::new(Mutex::new(None)),
                events,
                killer: None,
            }),
            phase: Phase::Running,
            snapshot: None,
            active: true,
            interrupt_ready: true,
            size: TerminalSize::default(),
        };
        (panel, controls)
    }

    fn snapshot_contents(panel: &TerminalPanel) -> String {
        panel
            .snapshot
            .as_ref()
            .map(|snapshot| {
                (0..snapshot.rows)
                    .flat_map(|row| {
                        (0..snapshot.cols).filter_map(move |col| snapshot.cell(row, col))
                    })
                    .map(vt100::Cell::contents)
                    .collect()
            })
            .unwrap_or_default()
    }

    #[test]
    fn ordinary_control_and_alt_keys_encode_as_terminal_input() {
        assert_eq!(
            encode_key(key(KeyCode::Char('x'), KeyModifiers::NONE), false),
            Some(b"x".to_vec())
        );
        assert_eq!(
            encode_key(key(KeyCode::Char('c'), KeyModifiers::CONTROL), false),
            Some(vec![0x03])
        );
        assert_eq!(
            encode_key(key(KeyCode::Char('x'), KeyModifiers::ALT), false),
            Some(b"\x1bx".to_vec())
        );
        assert_eq!(
            encode_key(key(KeyCode::Char('é'), KeyModifiers::NONE), false),
            Some("é".as_bytes().to_vec())
        );
    }

    #[test]
    fn navigation_keys_respect_application_cursor_and_modifiers() {
        assert_eq!(
            encode_key(key(KeyCode::Up, KeyModifiers::NONE), false),
            Some(b"\x1b[A".to_vec())
        );
        assert_eq!(
            encode_key(key(KeyCode::Up, KeyModifiers::NONE), true),
            Some(b"\x1bOA".to_vec())
        );
        assert_eq!(
            encode_key(key(KeyCode::Left, KeyModifiers::CONTROL), false),
            Some(b"\x1b[1;5D".to_vec())
        );
        assert_eq!(
            encode_key(key(KeyCode::BackTab, KeyModifiers::SHIFT), false),
            Some(b"\x1b[Z".to_vec())
        );
    }

    #[test]
    fn function_keys_cover_the_xterm_range() {
        assert_eq!(
            encode_key(key(KeyCode::F(1), KeyModifiers::NONE), false),
            Some(b"\x1bOP".to_vec())
        );
        assert_eq!(
            encode_key(key(KeyCode::F(5), KeyModifiers::NONE), false),
            Some(b"\x1b[15~".to_vec())
        );
        assert_eq!(
            encode_key(key(KeyCode::F(12), KeyModifiers::SHIFT), false),
            Some(b"\x1b[24;2~".to_vec())
        );
        assert_eq!(
            encode_key(key(KeyCode::F(13), KeyModifiers::NONE), false),
            None
        );
    }

    #[test]
    fn snapshot_keeps_only_the_visible_screen_and_input_modes() {
        let mut parser = vt100::Parser::new(2, 4, 20);
        parser.process(b"one\r\ntwo\r\nthree\x1b[?1h\x1b[?2004h");
        parser.screen_mut().set_scrollback(1);

        let snapshot = ScreenSnapshot::capture(parser.screen());
        assert_eq!(
            snapshot.cells.len(),
            8,
            "scrollback is not cloned into the frame"
        );
        assert_eq!(snapshot.scrollback, 1);
        assert!(snapshot.application_cursor);
        assert!(snapshot.bracketed_paste);
        assert_eq!(
            snapshot.cell(0, 0).map(vt100::Cell::contents),
            Some("t"),
            "the visible row follows the scrollback offset"
        );
    }

    #[test]
    fn terminal_queries_are_answered_without_entering_the_screen() {
        let (replies, receiver) = mpsc::channel();
        let mut parser =
            vt100::Parser::new_with_callbacks(24, 80, 0, TerminalCallbacks { replies });
        parser.process(b"\x1b[6n");
        let response = receiver.try_recv().expect("cursor query gets a reply");
        let Control::Reply(response) = response else {
            panic!("a terminal query must not look like user input");
        };
        assert_eq!(response, b"\x1b[1;1R");
        assert!(parser.screen().contents().is_empty());
    }

    #[test]
    fn paste_follows_the_childs_bracketed_paste_mode() {
        assert_eq!(paste_bytes("one\ntwo", false), b"one\ntwo");
        assert_eq!(paste_bytes("one\ntwo", true), b"\x1b[200~one\ntwo\x1b[201~");
    }

    #[test]
    fn one_interrupt_disarms_the_terminal_until_other_input_arrives() {
        let (mut panel, controls) = engaged_panel();

        assert_eq!(panel.handle_interrupt(), KeyOutcome::Consumed);
        let Control::Input(first) = controls.recv().expect("first interrupt is sent") else {
            panic!("the interrupt must be terminal input");
        };
        assert_eq!(first, [0x03]);
        assert_eq!(
            panel.handle_interrupt(),
            KeyOutcome::Ignored,
            "the next Ctrl+C must fall through to mirador"
        );
        assert!(
            matches!(controls.try_recv(), Err(TryRecvError::Empty)),
            "falling through must not send a second interrupt"
        );

        assert_eq!(
            panel.handle_key(key(KeyCode::PageUp, KeyModifiers::SHIFT)),
            KeyOutcome::Consumed
        );
        assert!(matches!(
            controls.recv().expect("scroll is sent"),
            Control::Scroll(_)
        ));
        assert_eq!(
            panel.handle_interrupt(),
            KeyOutcome::Consumed,
            "an intervening terminal action re-arms Ctrl+C"
        );
        assert!(matches!(
            controls.recv().expect("re-armed interrupt is sent"),
            Control::Input(bytes) if bytes == [0x03]
        ));
    }

    #[test]
    fn a_real_pty_resizes_interrupts_and_exits_without_blocking_the_panel() {
        let ready_marker = "mirador-sleep-ready";
        let marker = "mirador-terminal-probe";
        let (command, long_command, probe) = if cfg!(windows) {
            (
                vec!["cmd.exe".to_string(), "/Q".to_string(), "/D".to_string()],
                "echo mirador-sleep-re^ady & ping -n 31 127.0.0.1 >nul",
                "echo mirador-terminal-pro^be",
            )
        } else {
            (
                vec!["sh".to_string()],
                "printf 'mirador-sleep-%s\\n' ready; sleep 30",
                "printf 'mirador-terminal-pro%s\\n' be",
            )
        };
        let mut panel = TerminalPanel::new(&TerminalConfig {
            command,
            scrollback: 20,
        })
        .expect("launch worker starts");
        panel.update_size(Rect::new(0, 0, 43, 11));

        let deadline = Instant::now() + Duration::from_secs(10);
        let mut last_contents = String::new();
        let mut long_started = None;
        let mut ready_seen = None;
        let mut interrupt_sent = false;
        let mut marker_seen = false;
        let mut exit_sent = false;
        while Instant::now() < deadline {
            panel.tick();
            if panel.is_running() && long_started.is_none() {
                assert_eq!(
                    panel.handle_key(key(KeyCode::Enter, KeyModifiers::NONE)),
                    KeyOutcome::Consumed,
                    "Enter engages the running terminal"
                );
                let mut input = long_command.as_bytes().to_vec();
                input.extend_from_slice(&enter_sequence());
                assert!(panel.send(Control::Input(input)));
                long_started = Some(Instant::now());
            }
            last_contents = snapshot_contents(&panel);
            if ready_seen.is_none() && last_contents.contains(ready_marker) {
                ready_seen = Some(Instant::now());
            }
            if ready_seen.is_some_and(|seen| seen.elapsed() >= Duration::from_millis(200))
                && !interrupt_sent
            {
                assert_eq!(
                    panel.handle_interrupt(),
                    KeyOutcome::Consumed,
                    "Ctrl+C is delivered to the foreground process"
                );
                let mut input = probe.as_bytes().to_vec();
                input.extend_from_slice(&enter_sequence());
                assert!(panel.send(Control::Input(input)));
                interrupt_sent = true;
            }
            if interrupt_sent && !marker_seen && last_contents.contains(marker) {
                let snapshot = panel
                    .snapshot
                    .as_ref()
                    .expect("the marker came from a screen");
                assert_eq!(
                    (snapshot.cols, snapshot.rows),
                    (43, 11),
                    "a resize requested during launch must reach the child and parser"
                );
                marker_seen = true;
            }
            if marker_seen && panel.is_running() && !exit_sent {
                let mut input = b"exit 7".to_vec();
                input.extend_from_slice(&enter_sequence());
                assert!(panel.send(Control::Input(input)));
                exit_sent = true;
            }
            match &panel.phase {
                Phase::Exited(status) if marker_seen => {
                    assert_eq!(status, "code 7");
                    return;
                }
                Phase::Failed(error) => panic!("the PTY session failed: {error}"),
                Phase::Starting | Phase::Running | Phase::Exited(_) => {}
            }
            thread::sleep(Duration::from_millis(10));
        }
        panic!(
            "the PTY probe did not complete; long_started={}, ready_seen={}, \
             marker_seen={marker_seen}, phase={:?}, contents={last_contents:?}",
            long_started.is_some(),
            ready_seen.is_some(),
            panel.phase,
        );
    }
}
