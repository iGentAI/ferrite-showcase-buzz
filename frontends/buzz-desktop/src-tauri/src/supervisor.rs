//! Supervising local ACP harnesses.
//!
//! One child process per harness, spoken to over its stdio, with a reader thread
//! per process. Three properties are the reason this file exists rather than an
//! inline spawn:
//!
//! 1. **Every request the agent makes gets an answer.** An ACP request left
//!    unanswered wedges the agent forever — it is waiting on a pipe. So a method
//!    this shell does not implement is answered with a JSON-RPC error rather than
//!    ignored, and a permission request is answered by the broker.
//! 2. **The reader thread never blocks on a human.** A permission dialog is
//!    shown on its own thread, so the agent's output keeps draining while
//!    somebody decides — otherwise a dialog would deadlock the very stream that
//!    carries the agent's explanation of what it wants.
//! 3. **Death is reported.** When the process exits, its outstanding permission
//!    requests are refused and its status records the exit code, so "crashed" is
//!    distinguishable from "asked to stop".

use std::collections::VecDeque;
use std::io::{BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::Receiver;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

/// Each spawned process's generation. A harness id is reused across restarts,
/// so the threads reading one process name it by (id, generation): a reader
/// still unwinding after a stop must never act on the process that replaced it.
static NEXT_GENERATION: AtomicU64 = AtomicU64::new(1);

use buzz_desktop_core::acp::{self, AnswerBuffer, BoundedLine, Message, PermissionRequest};
use buzz_desktop_core::nostr::AnswerTarget;
use buzz_desktop_core::broker::{Decision, Outcome};
use buzz_desktop_core::harness::{Harness, Lifecycle, RuntimeStatus};
use buzz_desktop_core::{Error, Result};
use tauri::{AppHandle, Emitter, Manager};
use tauri_plugin_dialog::{DialogExt, MessageDialogButtons, MessageDialogKind};

use crate::state::{now_ms, AppState};

/// How many log lines one harness keeps.
///
/// A log a user can open must be bounded: an agent that loops printing is
/// otherwise an unbounded allocation in this process.
const LOG_LINES: usize = 500;
/// The longest log line kept; a longer one is cut at a character boundary and
/// says how long it was. Without it the 500 lines above are 500 lines of any
/// size.
const MAX_LOG_LINE_BYTES: usize = 4_096;

/// The event the UI can listen for. Not required for the shell to work — the
/// permission dialog is native — but it lets a client show activity.
pub const EVENT_AGENT_UPDATE: &str = "buzz-desktop://agent-update";
pub const EVENT_AGENT_STATUS: &str = "buzz-desktop://agent-status";

/// What this shell asked the agent, so a response can be recognised.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Awaiting {
    Initialize,
    SessionNew,
    /// A prompt, and where its answer goes when somebody is waiting for one.
    ///
    /// Carried HERE rather than looked up later because the answer arrives
    /// asynchronously, several notifications after the request, and by then
    /// nothing else remembers which room — or which thread — was waiting.
    Prompt { target: Option<AnswerTarget> },
}

struct Running {
    /// This spawn's [`NEXT_GENERATION`] value.
    generation: u64,
    child: Child,
    stdin: ChildStdin,
    status: RuntimeStatus,
    next_id: u64,
    pending: Vec<(u64, Awaiting)>,
    session: Option<String>,
    log: VecDeque<String>,
    /// Accumulated across `session/update` notifications because `session/prompt`
    /// answers with only a stop reason.
    answer: AnswerBuffer,
}

impl Running {
    fn write(&mut self, frame: &str) -> Result<()> {
        self.stdin
            .write_all(frame.as_bytes())
            .and_then(|()| self.stdin.flush())
            .map_err(|e| Error::Tool {
                program: self.status.id.clone(),
                detail: format!("could not write to the harness: {e}"),
            })
    }

    fn note(&mut self, line: &str) {
        if self.log.len() >= LOG_LINES {
            self.log.pop_front();
        }
        self.log.push_back(bounded_log_line(line));
    }
}

/// `line` cut to [`MAX_LOG_LINE_BYTES`] at a character boundary, saying how
/// long it was.
fn bounded_log_line(line: &str) -> String {
    if line.len() <= MAX_LOG_LINE_BYTES {
        return line.to_string();
    }
    let mut end = MAX_LOG_LINE_BYTES;
    while !line.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}… [truncated, {} bytes]", &line[..end], line.len())
}

#[derive(Default)]
pub struct Supervisor {
    running: Arc<Mutex<Vec<Running>>>,
}

impl Supervisor {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn with<T>(&self, f: impl FnOnce(&mut Vec<Running>) -> T) -> Result<T> {
        let mut running = self
            .running
            .lock()
            .map_err(|_| Error::Internal("the harness table is poisoned".into()))?;
        Ok(f(&mut running))
    }

    /// Whether `generation` is the process now running as `id`.
    fn is_generation(&self, id: &str, generation: u64) -> bool {
        self.with(|running| {
            running
                .iter()
                .any(|entry| entry.status.id == id && entry.generation == generation)
        })
        .unwrap_or(false)
    }

    pub fn is_running(&self, id: &str) -> bool {
        self.with(|running| running.iter().any(|entry| entry.status.id == id))
            .unwrap_or(false)
    }

    pub fn statuses(&self, configured: &[Harness]) -> Vec<RuntimeStatus> {
        let live = self
            .with(|running| {
                running
                    .iter()
                    .map(|entry| entry.status.clone())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        configured
            .iter()
            .map(|harness| {
                live.iter()
                    .find(|status| status.id == harness.id())
                    .cloned()
                    .unwrap_or_else(|| RuntimeStatus::stopped(harness))
            })
            .collect()
    }

    pub fn log(&self, id: &str) -> Vec<String> {
        self.with(|running| {
            running
                .iter()
                .find(|entry| entry.status.id == id)
                .map(|entry| entry.log.iter().cloned().collect())
                .unwrap_or_default()
        })
        .unwrap_or_default()
    }

    /// Start a local harness and drive it to a session.
    ///
    /// A remote harness has nothing to spawn: it is refused here by name rather
    /// than silently doing nothing, because "started" would then be a lie the UI
    /// displays.
    pub fn start(
        &self,
        app: &AppHandle,
        harness: &Harness,
        cwd: &std::path::Path,
        app_env: &[(String, String)],
    ) -> Result<RuntimeStatus> {
        let (command, args) = match harness {
            Harness::LocalStdio { command, args, .. } => (command.clone(), args.clone()),
            Harness::RemoteAcp { .. } => {
                return Err(Error::Invalid(
                    "a remote ACP harness is reached over HTTP; there is no local process to start"
                        .into(),
                ))
            }
        };
        harness.validate()?;
        if self.is_running(harness.id()) {
            return Err(Error::Invalid(format!(
                "{} is already running",
                harness.label()
            )));
        }

        let mut spawn = Command::new(&command);
        spawn
            .args(&args)
            .current_dir(cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        // The app's own variables are applied LAST (see `Harness::spawn_env`), so
        // a configured one cannot redirect the harness.
        for (key, value) in harness.spawn_env(app_env) {
            spawn.env(key, value);
        }
        let mut child = spawn.spawn().map_err(|e| Error::Tool {
            program: command.clone(),
            detail: format!("could not start the harness ({e}). Is {command} on PATH?"),
        })?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| Error::Internal("the harness has no stdin".into()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| Error::Internal("the harness has no stdout".into()))?;
        let stderr = child.stderr.take();
        let pid = child.id();

        let mut status = RuntimeStatus::stopped(harness);
        status.lifecycle = Lifecycle::Starting;
        status.pid = Some(pid);

        let generation = NEXT_GENERATION.fetch_add(1, Ordering::Relaxed);
        let entry = Running {
            generation,
            child,
            stdin,
            status: status.clone(),
            next_id: 1,
            pending: Vec::new(),
            session: None,
            log: VecDeque::new(),
            answer: AnswerBuffer::new(),
        };
        // Uniqueness is checked again where the entry is inserted: two starts
        // that both passed the check above must not both keep a process.
        let duplicate = self.with(move |running| {
            if running.iter().any(|existing| existing.status.id == entry.status.id) {
                return Some(entry);
            }
            running.push(entry);
            None
        })?;
        if let Some(mut duplicate) = duplicate {
            let _ = duplicate.child.kill();
            let _ = duplicate.child.wait();
            return Err(Error::Invalid(format!(
                "{} is already running",
                harness.label()
            )));
        }

        // Start reader threads before the handshake: if the first write fails
        // because the command exited immediately, the reader's EOF path removes
        // the entry instead of leaving it stuck in `Starting`.
        if let Some(stderr) = stderr {
            spawn_stderr_reader(
                stderr,
                Arc::clone(&self.running),
                harness.id().to_string(),
                generation,
            );
        }

        {
            let app = app.clone();
            let harness_id = harness.id().to_string();
            let cwd = cwd.to_path_buf();
            std::thread::spawn(move || {
                read_loop(app, harness_id, generation, stdout, cwd);
            });
        }

        let handshake = self.next_request_id(harness.id()).and_then(|id| {
            self.remember(harness.id(), id, Awaiting::Initialize)?;
            self.send(harness.id(), &acp::initialize_request(id))
        });
        if let Err(e) = handshake {
            // Every error after `spawn` cleans up, or the table keeps a child
            // nobody can stop.
            let detail = format!("the harness did not accept the handshake: {e}");
            if let Some(failed) = self.fail(harness.id(), &detail) {
                let _ = app.emit(EVENT_AGENT_STATUS, &failed);
            }
            return Err(e);
        }

        Ok(status)
    }

    /// Kill a harness and record it as FAILED.
    ///
    /// Distinct from [`Self::stop`] (which the user asked for) and from the
    /// reader's EOF path (which reports an exit): this is the shell giving up on
    /// a process it could not talk to, and the status says so.
    fn fail(&self, id: &str, detail: &str) -> Option<RuntimeStatus> {
        self.with(|running| {
            let at = running.iter().position(|entry| entry.status.id == id)?;
            let mut entry = running.remove(at);
            let _ = entry.child.kill();
            let code = entry.child.wait().ok().and_then(|status| status.code());
            let mut status = entry.status.clone();
            status.lifecycle = Lifecycle::Failed;
            status.pid = None;
            status.exit_code = code;
            status.detail = Some(detail.to_string());
            Some(status)
        })
        .ok()
        .flatten()
    }

    /// Stop a harness.
    ///
    /// A `session/cancel` first, so an agent mid-turn can stop cleanly, then the
    /// process is killed. Killing without the cancellation would leave a
    /// half-written file where the agent was working.
    pub fn stop(&self, id: &str) -> Result<RuntimeStatus> {
        let session = self.session(id);
        if let Some(session) = session {
            let _ = self.send(id, &acp::cancel_notification(&session));
        }
        self.with(|running| {
            let Some(at) = running.iter().position(|entry| entry.status.id == id) else {
                return Err(Error::Invalid(format!("{id} is not running")));
            };
            let mut entry = running.remove(at);
            let _ = entry.child.kill();
            let code = entry.child.wait().ok().and_then(|status| status.code());
            let mut status = entry.status.clone();
            status.lifecycle = Lifecycle::Stopped;
            status.pid = None;
            status.exit_code = code;
            Ok(status)
        })?
    }

    pub fn session(&self, id: &str) -> Option<String> {
        self.with(|running| {
            running
                .iter()
                .find(|entry| entry.status.id == id)
                .and_then(|entry| entry.session.clone())
        })
        .ok()
        .flatten()
    }

    /// Admit one prompt, ATOMICALLY.
    ///
    /// The pending check, the request id and the pending entry are one critical
    /// section on purpose: as three separate lock acquisitions, two concurrent
    /// commands could both find no prompt pending and both start a turn — exactly
    /// the interleaving the one-turn rule exists to prevent.
    ///
    /// One turn at a time is not a simplification either: the answer arrives as
    /// notifications carrying the SESSION but not the request, so two overlapping
    /// turns on one session cannot be told apart — their words would mix, and
    /// could be posted to the wrong room.
    ///
    /// Clearing the answer buffer belongs here for the same reason: a chunk that
    /// arrived while the harness was idle (a late notification from a turn that
    /// already failed, or an unsolicited one) must not be posted as part of the
    /// NEXT turn.
    fn admit_prompt(&self, id: &str, target: Option<&AnswerTarget>) -> Result<u64> {
        self.with(|running| {
            let Some(entry) = running.iter_mut().find(|entry| entry.status.id == id) else {
                return Err(Error::Invalid(format!("{id} is not running")));
            };
            let busy = entry
                .pending
                .iter()
                .any(|(_, awaiting)| matches!(awaiting, Awaiting::Prompt { .. }));
            if busy {
                return Err(Error::Invalid(format!(
                    "{id} is still answering the previous message; one turn at a time"
                )));
            }
            let request_id = entry.next_id;
            entry.next_id = entry.next_id.saturating_add(1);
            let _ = entry.answer.take();
            entry.pending.push((
                request_id,
                Awaiting::Prompt {
                    target: target.cloned(),
                },
            ));
            Ok(request_id)
        })?
    }

    /// Undo an admitted prompt whose frame could not be written.
    ///
    /// Without this a failed send leaves its pending entry behind and the harness
    /// is reported busy FOREVER — every later message refused, with a restart the
    /// only way out.
    fn withdraw_prompt(&self, id: &str, request_id: u64) {
        let _ = self.take_pending(id, request_id);
    }

    /// Send a prompt to a running harness.
    ///
    /// `target` is where the answer goes — the room, and the thread when the
    /// prompt came from one. `None` means nobody is waiting for it in a room: the
    /// answer then reaches the log and the UI event and goes no further, which is
    /// right for a prompt that did not come from a channel.
    ///
    /// Refused while the previous turn is still running (see [`Self::admit_prompt`]).
    pub fn prompt(
        &self,
        id: &str,
        text: &str,
        target: Option<&AnswerTarget>,
    ) -> Result<u64> {
        let Some(session) = self.session(id) else {
            return Err(Error::Invalid(format!(
                "{id} has not finished starting yet, so it has no session to prompt"
            )));
        };
        let request_id = self.admit_prompt(id, target)?;
        if let Err(e) = self.send(id, &acp::prompt_request(request_id, &session, text)) {
            self.withdraw_prompt(id, request_id);
            return Err(e);
        }
        Ok(request_id)
    }

    fn note_answer(&self, id: &str, text: &str) {
        let _ = self.with(|running| {
            if let Some(entry) = running.iter_mut().find(|entry| entry.status.id == id) {
                entry.answer.push(text);
            }
        });
    }

    fn take_answer(&self, id: &str) -> Option<String> {
        self.with(|running| {
            running
                .iter_mut()
                .find(|entry| entry.status.id == id)
                .and_then(|entry| entry.answer.take())
        })
        .ok()
        .flatten()
    }

    pub fn send(&self, id: &str, message: &serde_json::Value) -> Result<()> {
        let frame = acp::frame(message)?;
        self.with(|running| {
            let Some(entry) = running.iter_mut().find(|entry| entry.status.id == id) else {
                return Err(Error::Invalid(format!("{id} is not running")));
            };
            entry.write(&frame)
        })?
    }

    fn next_request_id(&self, id: &str) -> Result<u64> {
        self.with(|running| {
            let Some(entry) = running.iter_mut().find(|entry| entry.status.id == id) else {
                return Err(Error::Invalid(format!("{id} is not running")));
            };
            let next = entry.next_id;
            entry.next_id = entry.next_id.saturating_add(1);
            Ok(next)
        })?
    }

    fn remember(&self, id: &str, request_id: u64, awaiting: Awaiting) -> Result<()> {
        self.with(|running| {
            if let Some(entry) = running.iter_mut().find(|entry| entry.status.id == id) {
                entry.pending.push((request_id, awaiting));
            }
        })
    }

    fn take_pending(&self, id: &str, request_id: u64) -> Option<Awaiting> {
        self.with(|running| {
            running
                .iter_mut()
                .find(|entry| entry.status.id == id)
                .and_then(|entry| {
                    entry
                        .pending
                        .iter()
                        .position(|(pending, _)| *pending == request_id)
                        .map(|at| entry.pending.remove(at).1)
                })
        })
        .ok()
        .flatten()
    }

    fn set_session(&self, id: &str, session: &str) {
        let _ = self.with(|running| {
            if let Some(entry) = running.iter_mut().find(|entry| entry.status.id == id) {
                entry.session = Some(session.to_string());
                entry.status.lifecycle = Lifecycle::Running;
            }
        });
    }

    fn note(&self, id: &str, line: &str) {
        let _ = self.with(|running| {
            if let Some(entry) = running.iter_mut().find(|entry| entry.status.id == id) {
                entry.note(line);
            }
        });
    }

    /// Remove generation `generation` of `id` once its stream ended. A later
    /// generation running under the same id is left alone.
    fn finish(&self, id: &str, generation: u64, detail: Option<String>) -> Option<RuntimeStatus> {
        self.with(|running| {
            let at = running
                .iter()
                .position(|entry| entry.status.id == id && entry.generation == generation)?;
            let mut entry = running.remove(at);
            // The stream can end, or turn unreadable, while the process runs
            // on: a child that is removed from the table must not outlive it
            // unsupervised, and one that has exited must still be reaped.
            let code = match entry.child.try_wait() {
                Ok(Some(status)) => status.code(),
                _ => {
                    let _ = entry.child.kill();
                    entry.child.wait().ok().and_then(|status| status.code())
                }
            };
            let mut status = entry.status.clone();
            status.lifecycle = Lifecycle::Exited;
            status.pid = None;
            status.exit_code = code;
            status.detail = detail;
            Some(status)
        })
        .ok()
        .flatten()
    }
}

/// What deciding one permission request produced.
///
/// Three outcomes rather than an `Option`, because the CALLER has to answer the
/// agent and the right answer differs: a decision is sent as itself, an
/// already-resolved request was decided by the broker's own timeout (a refusal),
/// and a request that could not even be opened is a fault worth reporting as one.
pub enum Decided {
    Answered(Outcome),
    AlreadyResolved,
    CannotOpen(String),
}

/// Register a permission request and ask the person, WITHOUT blocking the caller.
///
/// The receiver yields exactly one [`Decided`]. Non-blocking because the REMOTE
/// (HTTP) driver must be able to stop waiting: its refusal has to be POSTed when
/// the broker's deadline passes, not whenever somebody happens to close the
/// dialog — the agent is holding a turn open in the meantime, and "fail closed"
/// that arrives an hour late is not fail closed.
pub fn ask_permission(
    app: &AppHandle,
    harness_id: &str,
    rpc_id: &serde_json::Value,
    request: PermissionRequest,
) -> Receiver<Decided> {
    let (tx, rx) = std::sync::mpsc::channel();
    // Opened on THIS thread, so a failure to register is reported before any
    // dialog appears and the caller can answer the agent immediately. The result
    // is bound INSIDE the block: a `MutexGuard` temporary at a block's tail
    // outlives the `State` handle it came from, which the borrow checker rightly
    // refuses.
    let opened = {
        let state = app.state::<AppState>();
        let outcome = match state.broker.lock() {
            Ok(mut broker) => broker
                .open(harness_id, rpc_id.clone(), request.clone(), now_ms())
                .map_err(|e| format!("{e}")),
            Err(_) => Err("the permission broker is unavailable".to_string()),
        };
        outcome
    };
    let key = match opened {
        Ok(key) => key,
        Err(detail) => {
            let _ = tx.send(Decided::CannotOpen(detail));
            return rx;
        }
    };

    let app = app.clone();
    let harness_id = harness_id.to_string();
    std::thread::spawn(move || {
        let allowed = app
            .dialog()
            .message(format!(
                "{} wants to run:\n\n{}\n\nAllow this once?",
                harness_id,
                request.title.as_deref().unwrap_or(&request.tool_name)
            ))
            .title("An agent is asking permission")
            .kind(MessageDialogKind::Warning)
            .buttons(MessageDialogButtons::OkCancelCustom(
                "Allow once".to_string(),
                "Refuse".to_string(),
            ))
            .blocking_show();
        let decision = if allowed {
            Decision::AllowOnce
        } else {
            Decision::Reject
        };
        let state = app.state::<AppState>();
        let decided = match state.broker.lock() {
            Ok(mut broker) => {
                match broker.answer(&key, &request.session_id, decision, now_ms()) {
                    Ok(outcome) => Decided::Answered(outcome),
                    // The broker resolved it while the dialog was open — its
                    // timeout, or the harness exiting. The first answer wins, and
                    // it was a refusal.
                    Err(_) => Decided::AlreadyResolved,
                }
            }
            Err(_) => Decided::CannotOpen("the permission broker is unavailable".to_string()),
        };
        // The receiver may be gone: the driver gave up waiting and has already
        // refused on the agent's behalf. Dropping this answer is correct.
        let _ = tx.send(decided);
    });
    rx
}

/// Ask, and WAIT for the answer.
///
/// The local path's shape: it is already on a per-request thread, and the broker's
/// expiry sweep can deliver a refusal over its stdio for it, so waiting here
/// costs nothing.
pub fn decide_permission(
    app: &AppHandle,
    harness_id: &str,
    rpc_id: &serde_json::Value,
    request: PermissionRequest,
) -> Decided {
    match ask_permission(app, harness_id, rpc_id, request).recv() {
        Ok(decided) => decided,
        // The dialog thread ended without answering. Fail closed.
        Err(_) => Decided::AlreadyResolved,
    }
}

/// Sweep expired permission requests, refusing each, and return the outcomes.
///
/// A LOCAL harness's refusal is delivered here, over its stdio. A REMOTE one's is
/// not: there is no pipe to write to, and its answer must ride a fresh POST — so
/// the remote driver watches for expiry itself (`remote::answer_for`) and sends
/// the refusal. Routing a remote outcome through the supervisor would silently
/// fail and leave the agent waiting, which is the bug this split closes.
pub fn expire_permissions(app: &AppHandle) -> Vec<Outcome> {
    let state = app.state::<AppState>();
    let expired = state
        .broker
        .lock()
        .map(|mut broker| broker.expire(now_ms()))
        .unwrap_or_default();
    for outcome in &expired {
        if state.supervisor.is_running(&outcome.harness_id) {
            respond(&state, outcome);
        }
    }
    expired
}

fn spawn_stderr_reader(
    stderr: std::process::ChildStderr,
    running: Arc<Mutex<Vec<Running>>>,
    id: String,
    generation: u64,
) {
    std::thread::spawn(move || {
        let mut reader = BufReader::new(stderr);
        while let Ok(read) = acp::read_line_bounded(&mut reader, MAX_LOG_LINE_BYTES) {
            let line = match read {
                BoundedLine::Eof => break,
                BoundedLine::Line(line) => format!("stderr: {line}"),
                BoundedLine::Oversized(bytes) => {
                    format!("stderr: [a line of {bytes} bytes, not kept]")
                }
            };
            if let Ok(mut table) = running.lock() {
                if let Some(entry) = table
                    .iter_mut()
                    .find(|entry| entry.status.id == id && entry.generation == generation)
                {
                    entry.note(&line);
                }
            }
        }
    });
}

fn read_loop(
    app: AppHandle,
    harness_id: String,
    generation: u64,
    stdout: std::process::ChildStdout,
    cwd: std::path::PathBuf,
) {
    let mut reader = BufReader::new(stdout);
    loop {
        let line = acp::read_line_bounded(&mut reader, acp::MAX_FRAME_BYTES);
        // A reader whose process was stopped, or replaced under the same id,
        // acts on nothing: every handler below addresses the harness by id.
        if !app
            .state::<AppState>()
            .supervisor
            .is_generation(&harness_id, generation)
        {
            return;
        }
        let line = match line {
            Ok(BoundedLine::Line(line)) => line,
            Ok(BoundedLine::Oversized(bytes)) => {
                let detail = format!(
                    "an ACP frame of {bytes} bytes is over the {}-byte limit; skipped",
                    acp::MAX_FRAME_BYTES
                );
                app.state::<AppState>().supervisor.note(&harness_id, &detail);
                let _ = app.emit(
                    EVENT_AGENT_UPDATE,
                    serde_json::json!({ "id": harness_id, "error": detail }),
                );
                continue;
            }
            Ok(BoundedLine::Eof) | Err(_) => break,
        };
        if line.trim().is_empty() {
            continue;
        }
        app.state::<AppState>().supervisor.note(&harness_id, &line);
        match acp::parse_line(&line) {
            Ok(Message::Request { id, method, params }) => {
                handle_request(&app, &harness_id, &id, &method, &params);
            }
            Ok(Message::Response { id, result }) => {
                handle_response(&app, &harness_id, &cwd, &id, result);
            }
            Ok(Message::Failure { id, message, .. }) => {
                if handle_failure(&app, &harness_id, &id, message) {
                    break;
                }
            }
            Ok(Message::Notification { method, params }) => {
                handle_notification(&app, &harness_id, method, params);
            }
            Err(e) => {
                app.state::<AppState>()
                    .supervisor
                    .note(&harness_id, &format!("unreadable frame: {e}"));
            }
        }
    }

    finish_stream(&app, &harness_id, generation);
}

fn handle_response(
    app: &AppHandle,
    harness_id: &str,
    cwd: &std::path::Path,
    id: &serde_json::Value,
    result: serde_json::Value,
) {
    let state = app.state::<AppState>();
    let request_id = id.as_u64().unwrap_or(0);
    match state.supervisor.take_pending(harness_id, request_id) {
        Some(Awaiting::Initialize) => {
            let meta_ok = acp::negotiated_binding_meta(&result);
            state.supervisor.note(
                harness_id,
                &format!("initialize: binding _meta negotiated = {meta_ok}"),
            );
            if let Ok(next) = state.supervisor.next_request_id(harness_id) {
                let _ = state
                    .supervisor
                    .remember(harness_id, next, Awaiting::SessionNew);
                let _ = state.supervisor.send(
                    harness_id,
                    &acp::session_new_request(next, &cwd.to_string_lossy(), None),
                );
            }
        }
        Some(Awaiting::SessionNew) => {
            if let Some(session) = result.get("sessionId").and_then(|v| v.as_str()) {
                state.supervisor.set_session(harness_id, session);
                let _ = app.emit(
                    EVENT_AGENT_STATUS,
                    serde_json::json!({ "id": harness_id, "lifecycle": "running" }),
                );
            }
        }
        Some(Awaiting::Prompt { target }) => {
            let answer = state.supervisor.take_answer(harness_id);
            let _ = app.emit(
                EVENT_AGENT_UPDATE,
                serde_json::json!({ "id": harness_id, "result": result }),
            );
            if let (Some(target), Some(answer)) = (target, answer) {
                // Post on a dedicated thread so the stdout reader keeps draining.
                spawn_answer_post(app.clone(), harness_id.to_string(), target, answer);
            }
        }
        None => {
            let _ = app.emit(
                EVENT_AGENT_UPDATE,
                serde_json::json!({ "id": harness_id, "result": result }),
            );
        }
    }
}

fn spawn_answer_post(
    app: AppHandle,
    harness_id: String,
    target: AnswerTarget,
    answer: String,
) {
    let room = target.channel.clone();
    std::thread::spawn(move || {
        let state = app.state::<AppState>();
        match crate::workspace::post_as_agent(&state, &harness_id, &target, &answer) {
            Ok(event_id) => {
                state
                    .supervisor
                    .note(&harness_id, &format!("posted answer as event {event_id}"));
                let _ = app.emit(
                    EVENT_AGENT_UPDATE,
                    serde_json::json!({
                        "id": harness_id,
                        "posted": event_id,
                        "channel": room,
                    }),
                );
            }
            Err(detail) => {
                state
                    .supervisor
                    .note(&harness_id, &format!("answer NOT posted: {detail}"));
                let _ = app.emit(
                    EVENT_AGENT_UPDATE,
                    serde_json::json!({
                        "id": harness_id,
                        "error": detail,
                    }),
                );
            }
        }
    });
}

fn handle_failure(
    app: &AppHandle,
    harness_id: &str,
    id: &serde_json::Value,
    message: String,
) -> bool {
    let state = app.state::<AppState>();
    let request_id = id.as_u64().unwrap_or(0);
    match state.supervisor.take_pending(harness_id, request_id) {
        Some(Awaiting::Initialize) | Some(Awaiting::SessionNew) => {
            let detail = format!("the harness refused the handshake: {message}");
            state.supervisor.note(harness_id, &detail);
            if let Some(failed) = state.supervisor.fail(harness_id, &detail) {
                let _ = app.emit(EVENT_AGENT_STATUS, &failed);
            }
            true
        }
        Some(Awaiting::Prompt { .. }) | None => {
            // Discard partial text so half an answer is never posted to the room.
            state.supervisor.take_answer(harness_id);
            let _ = app.emit(
                EVENT_AGENT_UPDATE,
                serde_json::json!({ "id": harness_id, "error": message }),
            );
            false
        }
    }
}

fn handle_notification(
    app: &AppHandle,
    harness_id: &str,
    method: String,
    params: serde_json::Value,
) {
    let state = app.state::<AppState>();
    if method == "session/update" {
        let current_session = state.supervisor.session(harness_id);
        if let Some(text) = acp::session_text_chunk(&params, current_session.as_deref()) {
            state.supervisor.note_answer(harness_id, &text);
        }
    }
    let _ = app.emit(
        EVENT_AGENT_UPDATE,
        serde_json::json!({ "id": harness_id, "method": method, "params": params }),
    );
}

fn finish_stream(app: &AppHandle, harness_id: &str, generation: u64) {
    // Refuse anything the exited process was waiting on so no dialog stays open
    // for an agent that no longer exists.
    let state = app.state::<AppState>();
    // A stopped process's requests were abandoned by the stop; a reader of a
    // replaced one must not abandon its successor's.
    if !state.supervisor.is_generation(harness_id, generation) {
        return;
    }
    let abandoned = state
        .broker
        .lock()
        .map(|mut broker| broker.abandon_harness(harness_id))
        .unwrap_or_default();
    let detail = (!abandoned.is_empty()).then(|| {
        format!(
            "{} permission request(s) were refused when the harness exited",
            abandoned.len()
        )
    });
    if let Some(status) = state.supervisor.finish(harness_id, generation, detail) {
        let _ = app.emit(EVENT_AGENT_STATUS, &status);
    }
}

/// Stop a local harness and refuse every permission request it left open, so
/// no dialog it raised can be answered onto the stdin of a process started
/// after it.
pub fn stop_local(state: &AppState, id: &str) -> Result<RuntimeStatus> {
    let status = state.supervisor.stop(id)?;
    if let Ok(mut broker) = state.broker.lock() {
        broker.abandon_harness(id);
    }
    Ok(status)
}

/// Answer one request the agent made of us.
///
/// EVERY request is answered. An ACP request left unanswered wedges the agent on
/// a pipe read, so a method this shell does not implement gets a JSON-RPC
/// "method not found" rather than silence.
fn handle_request(
    app: &AppHandle,
    harness_id: &str,
    rpc_id: &serde_json::Value,
    method: &str,
    params: &serde_json::Value,
) {
    let state = app.state::<AppState>();
    if method != "session/request_permission" {
        let _ = state.supervisor.send(
            harness_id,
            &acp::error_response(
                rpc_id,
                -32601,
                &format!("{method} is not implemented by the buzz desktop shell"),
            ),
        );
        return;
    }

    let request = match PermissionRequest::parse(params) {
        Ok(request) => request,
        Err(e) => {
            // A malformed permission request cannot be shown to anybody, so it
            // is refused rather than guessed at.
            let _ = state.supervisor.send(
                harness_id,
                &acp::error_response(rpc_id, -32602, &format!("{e}")),
            );
            return;
        }
    };

    // Only the session this process negotiated may ask (the broker compares a
    // request with its own copy, so this is the check that binds it).
    if state.supervisor.session(harness_id).as_deref() != Some(request.session_id.as_str()) {
        let _ = state.supervisor.send(
            harness_id,
            &acp::error_response(
                rpc_id,
                -32602,
                &format!(
                    "session {} is not this harness's session; the permission request is refused",
                    request.session_id
                ),
            ),
        );
        return;
    }

    // The dialog runs on its OWN thread so this reader keeps draining the
    // agent's output while somebody decides — a dialog on the reader thread
    // would deadlock the stream that carries the agent's own explanation.
    let app = app.clone();
    let harness_id = harness_id.to_string();
    let rpc_id = rpc_id.clone();
    std::thread::spawn(move || {
        let state = app.state::<AppState>();
        match decide_permission(&app, &harness_id, &rpc_id, request) {
            Decided::Answered(outcome) => respond(&state, &outcome),
            // Somebody already answered the agent — the expiry sweep, or the
            // harness exiting. Answering again would contradict the first.
            Decided::AlreadyResolved => {}
            Decided::CannotOpen(detail) => {
                let _ = state.supervisor.send(
                    &harness_id,
                    &acp::error_response(&rpc_id, -32603, &detail),
                );
            }
        }
    });
}

pub fn respond(state: &AppState, outcome: &Outcome) {
    // An answer is written only to the process whose session asked: one
    // decided after a restart belongs to a process that no longer exists.
    if state.supervisor.session(&outcome.harness_id).as_deref() != Some(outcome.session_id.as_str()) {
        return;
    }
    let frame = buzz_desktop_core::broker::response_for(outcome);
    let _ = state.supervisor.send(&outcome.harness_id, &frame);
}

/// Sweep expired permission requests, refusing each.
///
/// Runs on a timer thread rather than being checked lazily, because "nobody
/// answered" must become a refusal even if nothing else happens in the app: an
/// agent waiting on a dialog whose user walked away has to be told no.
pub fn start_expiry_thread(app: AppHandle) {
    std::thread::spawn(move || loop {
        std::thread::sleep(std::time::Duration::from_secs(2));
        expire_permissions(&app);
    });
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    /// A supervisor holding one running `sleep`, standing in for a harness.
    fn supervising_a_sleeper() -> (Supervisor, u32) {
        let mut child = Command::new("sleep")
            .arg("30")
            .stdin(Stdio::piped())
            .spawn()
            .expect("sleep starts");
        let stdin = child.stdin.take().expect("piped stdin");
        let pid = child.id();
        let supervisor = Supervisor::new();
        supervisor
            .with(|running| {
                running.push(Running {
                    generation: 7,
                    child,
                    stdin,
                    status: RuntimeStatus {
                        id: "sleeper".into(),
                        label: "sleeper".into(),
                        transport: "local_stdio".into(),
                        lifecycle: Lifecycle::Running,
                        pid: Some(pid),
                        exit_code: None,
                        detail: None,
                    },
                    next_id: 1,
                    pending: Vec::new(),
                    session: None,
                    log: VecDeque::new(),
                    answer: AnswerBuffer::new(),
                });
            })
            .expect("table");
        (supervisor, pid)
    }

    fn alive(pid: u32) -> bool {
        Command::new("kill")
            .args(["-0", &pid.to_string()])
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    }

    #[test]
    fn a_log_line_is_kept_to_its_bound() {
        let (supervisor, pid) = supervising_a_sleeper();
        let huge = "é".repeat(1 << 20);
        supervisor.note("sleeper", &huge);
        let log = supervisor.log("sleeper");
        assert!(log[0].len() < MAX_LOG_LINE_BYTES + 64, "{} bytes kept", log[0].len());
        assert!(log[0].contains("[truncated"));
        let _ = supervisor.stop("sleeper");
        assert!(!alive(pid));
    }

    #[test]
    fn a_harness_whose_stream_ended_is_not_left_running() {
        // The reader's end-of-stream path records the exit. A child that closed
        // or garbled its stdout without exiting must not outlive its entry.
        let (supervisor, pid) = supervising_a_sleeper();
        // A reader of an earlier generation removes nothing.
        assert!(supervisor.finish("sleeper", 6, None).is_none());
        assert!(supervisor.is_running("sleeper"));
        let status = supervisor.finish("sleeper", 7, None).expect("the entry is removed");
        assert_eq!(status.lifecycle, Lifecycle::Exited);
        assert!(!alive(pid), "the child is still running after its entry was removed");
    }
}
