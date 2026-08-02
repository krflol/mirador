//! Child lifecycle and bounded stdio workers for one external panel.

use std::collections::VecDeque;
use std::io::{self, BufRead, BufReader, Write};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver, SyncSender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crate::config::PluginConfig;

use super::{
    DEFAULT_REFRESH, HostMessage, MAX_MESSAGE_BYTES, MAX_WATCH_TEXT_BYTES, PROTOCOL_VERSION,
    PluginMessage, WireFrame,
};

const MAX_STDERR_BYTES: usize = 8 * 1024;
const EVENT_QUEUE: usize = 256;
const WATCH_QUEUE: usize = 64;
const SHUTDOWN_GRACE: Duration = Duration::from_millis(300);

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Phase {
    Starting,
    Running,
    Exited(String),
    Failed(String),
    Stopping,
}

pub(super) struct Shared {
    pub(super) generation: u64,
    pub(super) ready: bool,
    pub(super) phase: Phase,
    pub(super) title: Option<String>,
    pub(super) refresh: Duration,
    pub(super) frame: Option<WireFrame>,
    pub(super) notice: Option<String>,
    pub(super) stderr: String,
    pub(super) watch: VecDeque<String>,
}

impl Shared {
    pub(super) fn starting() -> Self {
        Self {
            generation: 0,
            ready: false,
            phase: Phase::Starting,
            title: None,
            refresh: DEFAULT_REFRESH,
            frame: None,
            notice: None,
            stderr: String::new(),
            watch: VecDeque::new(),
        }
    }

    pub(super) fn changed(&mut self) {
        self.generation = self.generation.wrapping_add(1);
    }

    pub(super) fn fail(&mut self, message: impl Into<String>) {
        self.phase = Phase::Failed(message.into());
        self.changed();
    }
}

enum SupervisorCommand {
    Shutdown,
    Abort,
}

pub(super) struct Runtime {
    pub(super) events: SyncSender<HostMessage>,
    supervisor: mpsc::Sender<SupervisorCommand>,
}

impl Runtime {
    pub(super) fn shutdown(&self) {
        let _ = self.events.try_send(HostMessage::Shutdown);
        let _ = self.supervisor.send(SupervisorCommand::Shutdown);
    }
}

pub(super) fn spawn_process(
    spec: &PluginConfig,
    shared: &Arc<Mutex<Shared>>,
) -> io::Result<Runtime> {
    let mut command = Command::new(&spec.command[0]);
    command
        .args(&spec.command[1..])
        .env("MIRADOR_PLUGIN_PROTOCOL", PROTOCOL_VERSION.to_string())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn()?;
    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| io::Error::other("missing stdin"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| io::Error::other("missing stdout"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| io::Error::other("missing stderr"))?;

    let (event_tx, event_rx) = mpsc::sync_channel(EVENT_QUEUE);
    let (supervisor_tx, supervisor_rx) = mpsc::channel();

    let writer_shared = Arc::clone(shared);
    let writer_supervisor = supervisor_tx.clone();
    thread::spawn(move || writer_loop(stdin, event_rx, &writer_shared, &writer_supervisor));

    let reader_shared = Arc::clone(shared);
    let reader_supervisor = supervisor_tx.clone();
    thread::spawn(move || reader_loop(stdout, &reader_shared, &reader_supervisor));

    let stderr_shared = Arc::clone(shared);
    thread::spawn(move || stderr_loop(stderr, &stderr_shared));

    let supervisor_shared = Arc::clone(shared);
    thread::spawn(move || supervisor_loop(&mut child, &supervisor_rx, &supervisor_shared));

    let cwd = std::env::current_dir()
        .map(|path| path.to_string_lossy().into_owned())
        .unwrap_or_default();
    let config = serde_json::to_value(&spec.config).unwrap_or(serde_json::Value::Null);
    let hello = HostMessage::Hello {
        protocol: PROTOCOL_VERSION,
        host_version: env!("CARGO_PKG_VERSION"),
        plugin: spec.id.clone(),
        config,
        cwd,
    };
    event_tx
        .try_send(hello)
        .map_err(|error| io::Error::other(format!("sending hello: {error}")))?;

    Ok(Runtime {
        events: event_tx,
        supervisor: supervisor_tx,
    })
}

fn writer_loop(
    mut stdin: impl Write,
    events: Receiver<HostMessage>,
    shared: &Arc<Mutex<Shared>>,
    supervisor: &mpsc::Sender<SupervisorCommand>,
) {
    for event in events {
        let result = serde_json::to_writer(&mut stdin, &event)
            .map_err(io::Error::other)
            .and_then(|()| stdin.write_all(b"\n"))
            .and_then(|()| stdin.flush());
        if let Err(error) = result {
            shared
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .fail(format!("writing to plugin: {error}"));
            let _ = supervisor.send(SupervisorCommand::Abort);
            break;
        }
        if matches!(event, HostMessage::Shutdown) {
            break;
        }
    }
}

fn reader_loop(
    stdout: impl io::Read,
    shared: &Arc<Mutex<Shared>>,
    supervisor: &mpsc::Sender<SupervisorCommand>,
) {
    let mut reader = BufReader::new(stdout);
    loop {
        let mut bytes = Vec::new();
        match read_limited_line(&mut reader, &mut bytes) {
            Ok(0) => break,
            Ok(_) => {}
            Err(error) => {
                shared
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .fail(format!("reading plugin output: {error}"));
                let _ = supervisor.send(SupervisorCommand::Abort);
                break;
            }
        }
        while matches!(bytes.last(), Some(b'\n' | b'\r')) {
            bytes.pop();
        }
        let message: PluginMessage = match serde_json::from_slice(&bytes) {
            Ok(message) => message,
            Err(error) => {
                shared
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .fail(format!("invalid plugin message: {error}"));
                let _ = supervisor.send(SupervisorCommand::Abort);
                break;
            }
        };
        if !apply_message(message, shared) {
            let _ = supervisor.send(SupervisorCommand::Abort);
            break;
        }
    }
}

pub(super) fn read_limited_line(
    reader: &mut impl BufRead,
    target: &mut Vec<u8>,
) -> io::Result<usize> {
    let mut limited = io::Read::take(
        reader,
        u64::try_from(MAX_MESSAGE_BYTES + 1).unwrap_or(u64::MAX),
    );
    let read = limited.read_until(b'\n', target)?;
    if read > MAX_MESSAGE_BYTES || (read == MAX_MESSAGE_BYTES && !target.ends_with(b"\n")) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("message exceeds {MAX_MESSAGE_BYTES} bytes"),
        ));
    }
    Ok(read)
}

pub(super) fn apply_message(message: PluginMessage, shared: &Arc<Mutex<Shared>>) -> bool {
    let mut shared = shared
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    match message {
        PluginMessage::Ready {
            protocol,
            title,
            refresh_ms,
        } => {
            if protocol != PROTOCOL_VERSION {
                shared.fail(format!(
                    "protocol {protocol} is incompatible with host protocol {PROTOCOL_VERSION}"
                ));
                return false;
            }
            if shared.ready {
                shared.fail("plugin sent `ready` more than once");
                return false;
            }
            shared.ready = true;
            shared.phase = Phase::Running;
            shared.title = title;
            shared.refresh = refresh_ms.map_or(DEFAULT_REFRESH, |milliseconds| {
                Duration::from_millis(milliseconds.clamp(16, 60_000))
            });
            shared.changed();
        }
        PluginMessage::Frame {
            revision,
            title,
            counter,
            lines,
            bindings,
            input,
            cursor,
        } => {
            if !shared.ready {
                shared.fail("plugin sent a frame before `ready`");
                return false;
            }
            if shared
                .frame
                .as_ref()
                .is_some_and(|frame| revision <= frame.revision)
            {
                return true;
            }
            shared.frame = Some(WireFrame {
                revision,
                title,
                counter,
                lines,
                bindings,
                input,
                cursor,
            });
            shared.notice = None;
            shared.changed();
        }
        PluginMessage::Error { message, fatal } => {
            if fatal {
                shared.fail(message);
                return false;
            }
            shared.notice = Some(message);
            shared.changed();
        }
        PluginMessage::Watch { text } => {
            if !shared.ready {
                shared.fail("plugin sent a watch event before `ready`");
                return false;
            }
            if text.trim().is_empty()
                || text.len() > MAX_WATCH_TEXT_BYTES
                || text.chars().any(char::is_control)
            {
                shared.fail(format!(
                    "plugin watch text must be one non-empty line of at most {MAX_WATCH_TEXT_BYTES} UTF-8 bytes"
                ));
                return false;
            }
            if shared.watch.len() == WATCH_QUEUE {
                shared.watch.pop_front();
                shared.notice = Some("plugin watch queue overflowed; oldest event dropped".into());
            }
            shared.watch.push_back(text);
            shared.changed();
        }
    }
    true
}

fn stderr_loop(stderr: impl io::Read, shared: &Arc<Mutex<Shared>>) {
    let reader = BufReader::new(stderr);
    for line in reader.lines().map_while(Result::ok) {
        let mut shared = shared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !shared.stderr.is_empty() {
            shared.stderr.push('\n');
        }
        shared.stderr.push_str(&line);
        if shared.stderr.len() > MAX_STDERR_BYTES {
            let excess = shared.stderr.len() - MAX_STDERR_BYTES;
            let mut boundary = excess;
            while boundary < shared.stderr.len() && !shared.stderr.is_char_boundary(boundary) {
                boundary += 1;
            }
            shared.stderr.drain(..boundary);
        }
        shared.changed();
    }
}

fn supervisor_loop(
    child: &mut Child,
    commands: &Receiver<SupervisorCommand>,
    shared: &Arc<Mutex<Shared>>,
) {
    let mut deadline = None;
    loop {
        match commands.try_recv() {
            Ok(SupervisorCommand::Abort) => {
                let _ = child.kill();
                let _ = child.wait();
                return;
            }
            Ok(SupervisorCommand::Shutdown) => {
                deadline = Some(Instant::now() + SHUTDOWN_GRACE);
                let mut shared = shared
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                shared.phase = Phase::Stopping;
                shared.changed();
            }
            Err(TryRecvError::Disconnected) if deadline.is_none() => {
                deadline = Some(Instant::now() + SHUTDOWN_GRACE);
            }
            Err(TryRecvError::Empty | TryRecvError::Disconnected) => {}
        }

        match child.try_wait() {
            Ok(Some(status)) => {
                let mut shared = shared
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if !matches!(shared.phase, Phase::Stopping | Phase::Failed(_)) {
                    shared.phase = Phase::Exited(status.to_string());
                    shared.changed();
                }
                return;
            }
            Ok(None) => {}
            Err(error) => {
                shared
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .fail(format!("checking plugin process: {error}"));
                return;
            }
        }

        if deadline.is_some_and(|when| Instant::now() >= when) {
            let _ = child.kill();
            let _ = child.wait();
            return;
        }
        thread::sleep(Duration::from_millis(20));
    }
}
