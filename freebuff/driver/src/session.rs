//! The session: one Freebuff TUI in a pseudo terminal, one chat at a time.
//!
//! The driver is a translator, not an agent. It types the prompt into the TUI,
//! then reports what Freebuff writes into its own chat store: assistant text,
//! tool calls and their results, and the end of a turn. It never invents a
//! model, a cost, a context figure or an approval.

use std::collections::HashSet;
use std::io::BufRead;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant, SystemTime};

use serde_json::Value;

use crate::chatdir::{self, ChatDir};
use crate::logtail::{LogTail, TurnSignal};
use crate::probe;
use crate::protocol::{parse_command, Command, Events};
use crate::ptyproc::PtyProc;
use crate::transcript::{self, TranscriptState};

/// How often the chat store is polled, as the design specifies.
const POLL: Duration = Duration::from_millis(250);
/// Quiet time the TUI needs after its first paint before the first prompt.
const SETTLE_QUIET: Duration = Duration::from_millis(800);
/// Upper bound on that wait, in case the TUI animates without pause.
const SETTLE_LIMIT: Duration = Duration::from_secs(8);
/// How long the driver waits for a chat directory before typing anyway.
const CHAT_DIR_WAIT: Duration = Duration::from_secs(20);
/// How long an interrupt may take before the driver reports it as done.
const INTERRUPT_LIMIT: Duration = Duration::from_secs(5);
/// When the "cancel with Ctrl-C" fallback is sent inside that window.
const INTERRUPT_CTRL_C_AFTER: Duration = Duration::from_millis(1200);
/// How quiet the TUI must look before an interrupt counts as settled.
const INTERRUPT_SETTLE: Duration = Duration::from_millis(800);
/// No transcript progress for this long gets a diagnostic with the screen text.
const NO_ACTIVITY_NOTE: Duration = Duration::from_secs(30);
/// How much of the stripped screen a diagnostic quotes.
const SCREEN_TAIL: usize = 1200;

/// How long a turn may make no transcript progress before the driver quotes
/// the screen. `FREEBUFF_DRIVER_ACTIVITY_NOTE_SECS` shortens it while an author
/// is chasing a TUI that is waiting for something; it changes no event.
fn activity_note_limit() -> Duration {
    std::env::var("FREEBUFF_DRIVER_ACTIVITY_NOTE_SECS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .map(Duration::from_secs)
        .filter(|limit| *limit >= Duration::from_secs(2))
        .unwrap_or(NO_ACTIVITY_NOTE)
}

/// Bracketed paste: the TUI inserts the text.
fn bracketed_paste(text: &str) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(text.len() + 16);
    bytes.extend_from_slice(b"\x1b[200~");
    bytes.extend_from_slice(text.as_bytes());
    bytes.extend_from_slice(b"\x1b[201~");
    bytes
}

#[derive(Debug)]
struct InterruptState {
    started: Instant,
    ctrl_c_sent: bool,
}

struct Session {
    events: Events,
    cwd: Option<PathBuf>,
    resumed_id: Option<String>,
    model: Option<Value>,
    permission_mode: Option<Value>,
    pty: Option<PtyProc>,
    spawned_at: SystemTime,
    known: HashSet<String>,
    projects: Option<PathBuf>,
    chat: Option<ChatDir>,
    transcript: TranscriptState,
    transcript_stamp: Option<(SystemTime, u64)>,
    log_tail: Option<LogTail>,
    initialized: bool,
    turn_open: bool,
    pending_prompt: Option<String>,
    settled: bool,
    settled_at: Option<Instant>,
    started_at: Instant,
    last_output: Instant,
    seen_output: bool,
    last_progress: Instant,
    last_activity_note: Option<Instant>,
    screen: String,
    interrupting: Option<InterruptState>,
    eof_since: Option<Instant>,
    prompt_count: u32,
}

impl Session {
    fn new() -> Self {
        let now = Instant::now();
        Self {
            events: Events::new(),
            cwd: None,
            resumed_id: None,
            model: None,
            permission_mode: None,
            pty: None,
            spawned_at: SystemTime::now(),
            known: HashSet::new(),
            projects: chatdir::projects_root(),
            chat: None,
            transcript: TranscriptState::new(),
            transcript_stamp: None,
            log_tail: None,
            initialized: false,
            turn_open: false,
            pending_prompt: None,
            settled: false,
            settled_at: None,
            started_at: now,
            last_output: now,
            seen_output: false,
            last_progress: now,
            last_activity_note: None,
            screen: String::new(),
            interrupting: None,
            eof_since: None,
            prompt_count: 0,
        }
    }

    /// stderr and the protocol's own `stderr` event: the console shows the
    /// second one, a person reading a log gets both.
    fn diag(&mut self, message: &str) {
        eprintln!("freebuff-driver: {message}");
        let _ = self.events.stderr_line(message);
    }

    /// Report a failure that ends the session before it started: the console
    /// gets a `closed` event, which is what a gone CLI looks like.
    fn fail_closed(&mut self, message: &str) -> ! {
        self.diag(message);
        let _ = self.events.closed();
        std::process::exit(1);
    }

    fn finish(&mut self, code: i32) -> ! {
        if let Some(pty) = self.pty.as_mut() {
            pty.kill();
        }
        // Flush the event stream before the process disappears. Exit without
        // running destructors: closing the pseudo console from under a live
        // CLI is the one teardown that can block.
        let _ = self.events.closed();
        std::process::exit(code);
    }

    fn handle_start(&mut self, command: Command) {
        match command {
            Command::Spawn {
                cwd,
                model,
                permission_mode,
                resume,
                ..
            } => {
                if self.pty.is_some() {
                    self.diag("spawn was already answered; this driver runs one chat");
                    return;
                }
                self.start(&cwd, model, permission_mode, resume);
            }
            Command::Prompt { text } => self.prompt(text),
            Command::Interrupt => self.interrupt(),
            Command::Answer { .. } => self.diag(
                "Freebuff asks no approvals, so this driver has no approval to answer; \
                 the message was ignored",
            ),
            Command::Settings {
                model,
                effort,
                fast,
                permission_mode,
            } => {
                let mut asked: Vec<&str> = Vec::new();
                if model.is_some() {
                    asked.push("model");
                }
                if effort.is_some() {
                    asked.push("effort");
                }
                if fast.is_some() {
                    asked.push("fast mode");
                }
                if permission_mode.is_some() {
                    asked.push("permission mode");
                }
                if asked.is_empty() {
                    self.diag("settings carried nothing to apply");
                } else {
                    self.diag(&format!(
                        "a running Freebuff chat cannot change {}; stop the chat and resume it \
                         to start again with other settings",
                        asked.join(", ")
                    ));
                }
            }
            Command::Shutdown => {
                self.diag("shutdown requested; ending the Freebuff session");
                self.finish(0);
            }
        }
    }

    fn start(
        &mut self,
        cwd: &str,
        model: Option<Value>,
        permission_mode: Option<Value>,
        resume: Option<String>,
    ) {
        let cwd_path = PathBuf::from(cwd);
        if !cwd_path.is_dir() {
            self.fail_closed(&format!("the chat directory {cwd} does not exist"));
        }

        let exe = match probe::freebuff_path() {
            Some(exe) => exe,
            None => {
                let expected = probe::manicode_dir()
                    .map(|dir| {
                        dir.join(if cfg!(windows) {
                            "freebuff.exe"
                        } else {
                            "freebuff"
                        })
                    })
                    .map(|path| path.display().to_string())
                    .unwrap_or_else(|| "~/.config/manicode/freebuff".to_string());
                self.fail_closed(&format!("Freebuff CLI not found at {expected}"));
            }
        };

        match probe::read_version(&exe) {
            Ok(version) => {
                if !probe::version_at_least(&version, probe::MIN_VERSION) {
                    self.fail_closed(&format!(
                        "Freebuff CLI {version} is older than the {}.{}.{} this driver needs; \
                         update Freebuff and try again",
                        probe::MIN_VERSION.0,
                        probe::MIN_VERSION.1,
                        probe::MIN_VERSION.2
                    ));
                }
                self.diag(&format!("Freebuff {version} at {}", exe.display()));
            }
            Err(reason) => self.fail_closed(&reason),
        }

        match probe::credentials_present() {
            Some(false) => self.fail_closed(
                "Freebuff is not signed in; run `freebuff login` once in a terminal, \
                 then start the chat again",
            ),
            None => self.diag("could not locate Freebuff's config directory"),
            Some(true) => {}
        }

        let resume = resume.filter(|id| !id.trim().is_empty());
        let mut args: Vec<String> = vec!["--cwd".to_string(), cwd_path.display().to_string()];
        if let Some(id) = &resume {
            args.push("--continue".to_string());
            args.push(id.clone());
        }

        self.known = self
            .projects
            .as_deref()
            .map(chatdir::known_ids)
            .unwrap_or_default();
        self.spawned_at = SystemTime::now();
        self.cwd = Some(cwd_path.clone());
        self.resumed_id = resume.clone();
        self.model = model;
        self.permission_mode = permission_mode;

        let pty = match PtyProc::spawn(&exe, &args, &cwd_path) {
            Ok(pty) => pty,
            Err(reason) => self.fail_closed(&reason),
        };
        self.diag(&format!(
            "started Freebuff (pid {}) in {} at {}x{}",
            pty.pid()
                .map(|pid| pid.to_string())
                .unwrap_or_else(|| "?".to_string()),
            cwd_path.display(),
            crate::ptyproc::COLS,
            crate::ptyproc::ROWS
        ));
        self.pty = Some(pty);
        self.started_at = Instant::now();
        self.last_output = Instant::now();
    }

    fn prompt(&mut self, text: String) {
        if self.pty.is_none() {
            self.diag("prompt before spawn; ignored");
            return;
        }
        if self.turn_open {
            self.diag("a Freebuff turn is already running; this driver takes one prompt at a time");
            return;
        }
        if text.trim().is_empty() {
            self.diag("prompt was empty; ignored");
            return;
        }
        if self.pending_prompt.is_some() {
            self.diag("a prompt is already waiting for the TUI to settle; ignored");
            return;
        }
        self.pending_prompt = Some(text);
    }

    fn interrupt(&mut self) {
        if self.pty.is_none() {
            self.diag("interrupt before spawn; ignored");
            return;
        }
        if !self.turn_open {
            self.diag("interrupt with no turn running; ignored");
            return;
        }
        if self.interrupting.is_some() {
            return;
        }
        self.diag("interrupt: sending Escape to Freebuff");
        if let Some(pty) = self.pty.as_ref() {
            pty.send(b"\x1b");
        }
        self.interrupting = Some(InterruptState {
            started: Instant::now(),
            ctrl_c_sent: false,
        });
    }

    /// Send the waiting prompt once the TUI has painted and settled, and once
    /// the chat directory is known (or after the bounded wait).
    fn maybe_send_prompt(&mut self) {
        let Some(text) = self.pending_prompt.clone() else {
            return;
        };
        if !self.settled {
            return;
        }
        let settled_for = self.settled_at.map(|at| at.elapsed()).unwrap_or_default();
        if self.chat.is_none() && settled_for < CHAT_DIR_WAIT {
            return;
        }
        if self.chat.is_none() {
            self.diag(
                "no Freebuff chat directory appeared; sending the prompt anyway \
                 (resume will not work for this chat)",
            );
        }
        self.pending_prompt = None;
        if let Some(pty) = self.pty.as_ref() {
            pty.send(&bracketed_paste(&text));
            std::thread::sleep(Duration::from_millis(250));
            pty.send(b"\r");
        }
        self.prompt_count += 1;
        self.turn_open = true;
        self.last_progress = Instant::now();
        self.last_activity_note = None;
    }

    /// Read whatever the TUI has printed and keep a stripped tail of it.
    fn pump_pty(&mut self) {
        let Some(pty) = self.pty.as_ref() else {
            return;
        };
        let bytes = pty.drain();
        if bytes.is_empty() {
            return;
        }
        self.seen_output = true;
        self.last_output = Instant::now();
        self.screen.push_str(&crate::ptyproc::strip_ansi(&bytes));
        if self.screen.len() > 8 * SCREEN_TAIL {
            let cut = self.screen.len() - 4 * SCREEN_TAIL;
            let mut start = cut;
            while start < self.screen.len() && !self.screen.is_char_boundary(start) {
                start += 1;
            }
            self.screen = self.screen[start..].to_string();
        }
        if self.interrupting.is_some() {
            // Any repaint counts as the TUI still working on the cancel.
            self.last_progress = Instant::now();
        }
    }

    fn update_settled(&mut self) {
        if self.settled {
            return;
        }
        let painted = self.seen_output;
        let quiet = self.last_output.elapsed() >= SETTLE_QUIET;
        let waited = self.started_at.elapsed() >= SETTLE_LIMIT;
        if (painted && quiet) || waited {
            self.settled = true;
            self.settled_at = Some(Instant::now());
            if painted {
                self.diag("the Freebuff TUI painted and went quiet");
            } else {
                self.diag("the Freebuff TUI printed nothing yet; typing anyway");
            }
        }
    }

    fn poll_chat(&mut self) {
        if self.chat.is_none() {
            let Some(projects) = self.projects.clone() else {
                return;
            };
            let Some(cwd) = self.cwd.clone() else {
                return;
            };
            let found = chatdir::find_started_chat(
                &projects,
                &cwd,
                self.spawned_at,
                &self.known,
                self.resumed_id.as_deref(),
            );
            let Some(found) = found else {
                return;
            };
            self.diag(&format!("chat {}", found.id));
            self.log_tail = Some(LogTail::new(found.dir.join("log.jsonl")));
            self.chat = Some(found);
        }

        let Some(chat) = self.chat.as_ref() else {
            return;
        };
        let dir = chat.dir.clone();

        if !self.initialized {
            self.initialized = true;
            let session_ref = chat.id.clone();
            let model = self.model.clone();
            let permission_mode = self.permission_mode.clone();
            let _ = self
                .events
                .initialized(&session_ref, model.as_ref(), permission_mode.as_ref());
        }

        self.poll_transcript(&dir, false);
        self.poll_log();
    }

    fn poll_transcript(&mut self, dir: &Path, force: bool) {
        let path = dir.join("chat-messages.json");
        let stamp = std::fs::metadata(&path)
            .ok()
            .and_then(|meta| meta.modified().ok().map(|when| (when, meta.len())));
        if !force && stamp == self.transcript_stamp {
            return;
        }
        let Ok(text) = std::fs::read_to_string(&path) else {
            return;
        };
        let Ok(messages) = transcript::parse_messages(&text) else {
            // A half-written file: try again on the next poll.
            return;
        };
        self.transcript_stamp = stamp;

        let events = self.transcript.ingest(&messages);
        if events.is_empty() {
            return;
        }
        self.last_progress = Instant::now();
        self.last_activity_note = None;
        for event in events {
            match event {
                transcript::BlockEvent::Text(text) => {
                    let _ = self.events.text(&text);
                }
                transcript::BlockEvent::TextDelta(text) => {
                    let _ = self.events.text_delta(&text);
                }
                transcript::BlockEvent::Reasoning(text) => {
                    let _ = self.events.reasoning(&text);
                }
                transcript::BlockEvent::ReasoningDelta(text) => {
                    let _ = self.events.reasoning_delta(&text);
                }
                transcript::BlockEvent::ToolStarted {
                    tool_use_id,
                    name,
                    input,
                } => {
                    let _ = self.events.tool_started(&tool_use_id, &name, &input);
                }
                transcript::BlockEvent::ToolResult {
                    tool_use_id,
                    name,
                    text,
                } => {
                    // Freebuff does not mark a tool result as an error; the
                    // driver does not invent one.
                    let _ = self.events.tool_result(&tool_use_id, &name, &text, false);
                }
            }
        }
    }

    fn poll_log(&mut self) {
        let Some(tail) = self.log_tail.as_mut() else {
            return;
        };
        let signals = tail.poll();
        for signal in signals {
            match signal {
                TurnSignal::Finished => self.end_turn(None),
                TurnSignal::Failed(message) => self.end_turn(Some(message)),
            }
        }
    }

    /// Close the open turn. `failure` is what the log said, when it said
    /// something went wrong.
    fn end_turn(&mut self, failure: Option<String>) {
        if !self.turn_open {
            // A marker without an open turn is history (a resumed chat) or an
            // extra line from a turn already reported.
            return;
        }
        if let Some(chat) = self.chat.as_ref() {
            let dir = chat.dir.clone();
            // Late text lands after the marker sometimes; read once more.
            self.poll_transcript(&dir, true);
        }
        self.turn_open = false;
        self.interrupting = None;

        match failure {
            Some(message) => {
                self.diag(&format!("turn failed: {message}"));
                let _ = self
                    .events
                    .turn_completed("error", "failed", true, Some(&message));
            }
            None => {
                let _ = self
                    .events
                    .turn_completed("finished", "completed", false, None);
            }
        }
    }

    /// The interrupt path: Escape, then Ctrl-C if the TUI ignores it, then one
    /// reported end. This is the only end the driver synthesizes, and it never
    /// claims success.
    fn drive_interrupt(&mut self) {
        let Some(state) = self.interrupting.as_ref() else {
            return;
        };
        let elapsed = state.started.elapsed();
        let quiet = self.last_output.elapsed();

        if !state.ctrl_c_sent && elapsed >= INTERRUPT_CTRL_C_AFTER && quiet < SETTLE_QUIET {
            self.diag("interrupt: the TUI is still working; sending Ctrl-C");
            if let Some(pty) = self.pty.as_ref() {
                pty.send(b"\x03");
            }
            if let Some(state) = self.interrupting.as_mut() {
                state.ctrl_c_sent = true;
            }
        }

        let settled = elapsed >= INTERRUPT_CTRL_C_AFTER && quiet >= INTERRUPT_SETTLE;
        if elapsed < INTERRUPT_LIMIT && !settled {
            return;
        }

        if let Some(chat) = self.chat.as_ref() {
            let dir = chat.dir.clone();
            self.poll_transcript(&dir, true);
        }
        self.interrupting = None;
        self.turn_open = false;
        if settled {
            self.diag("interrupt: the TUI settled");
        } else {
            self.diag("interrupt: no end marker within 5 seconds; reporting the turn stopped");
        }
        // The user asked to stop, so this is not an error turn, and it is not
        // a completed one either.
        let _ = self
            .events
            .turn_completed("interrupted", "interrupted", false, None);
    }

    fn note_no_activity(&mut self) {
        if !self.turn_open || self.interrupting.is_some() {
            return;
        }
        let limit = activity_note_limit();
        if self.last_progress.elapsed() < limit {
            return;
        }
        if let Some(at) = self.last_activity_note {
            if at.elapsed() < limit {
                return;
            }
        }
        self.last_activity_note = Some(Instant::now());

        if self.screen.contains("Can't reach Freebuff's servers")
            || self.screen.contains("Can\u{2019}t reach Freebuff\u{2019}s servers")
            || self.screen.contains("Try another network")
        {
            self.end_turn(Some(
                "Can't reach Freebuff's servers. Retrying… Try another network.".to_string(),
            ));
            return;
        }

        let screen = self.screen_tail();
        if screen.is_empty() {
            self.diag(&format!(
                "no transcript activity for {} seconds and the TUI has printed nothing readable",
                limit.as_secs()
            ));
        } else {
            self.diag(&format!(
                "no transcript activity for {} seconds; the TUI is showing: {screen}",
                limit.as_secs()
            ));
        }
    }

    /// The last few readable lines of the screen, for a diagnostic. Freebuff
    /// can put a prompt here that the driver cannot answer; quoting it is
    /// better than answering it blindly.
    fn screen_tail(&self) -> String {
        let mut lines: Vec<&str> = self
            .screen
            .lines()
            .rev()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .take(12)
            .collect();
        lines.reverse();
        let mut text = lines.join(" | ");
        if text.chars().count() > SCREEN_TAIL {
            let start = text
                .char_indices()
                .nth(text.chars().count().saturating_sub(SCREEN_TAIL))
                .map(|(index, _)| index)
                .unwrap_or(0);
            text = text[start..].to_string();
        }
        text
    }

    fn check_exit(&mut self) {
        let Some(pty) = self.pty.as_mut() else {
            return;
        };
        let status = pty.try_wait();
        let eof = pty.eof();
        if eof && self.eof_since.is_none() {
            self.eof_since = Some(Instant::now());
        }
        let finished = status.is_some()
            || self
                .eof_since
                .map(|at| at.elapsed() >= Duration::from_secs(2))
                .unwrap_or(false);
        if !finished {
            return;
        }

        let code = status
            .map(|code| code.to_string())
            .unwrap_or_else(|| "unknown".to_string());
        if let Some(error) = pty.reader_error() {
            self.diag(&format!("the pseudo terminal stopped: {error}"));
        }
        self.diag(&format!("Freebuff exited (code {code})"));

        if self.turn_open {
            // The CLI died mid-turn: that turn did not finish.
            if let Some(chat) = self.chat.as_ref() {
                let dir = chat.dir.clone();
                self.poll_transcript(&dir, true);
            }
            self.turn_open = false;
            let _ = self.events.turn_completed(
                "error",
                "failed",
                true,
                Some("Freebuff exited before the turn finished"),
            );
        }
        self.pty = None;
        self.finish(0);
    }
}

/// Read stdin on its own thread: a blocked read must not stall the session.
fn spawn_stdin_reader() -> mpsc::Receiver<String> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let stdin = std::io::stdin();
        for line in stdin.lock().lines() {
            match line {
                Ok(line) => {
                    if tx.send(line).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });
    rx
}

pub fn run() -> Result<(), String> {
    let mut session = Session::new();
    let commands = spawn_stdin_reader();
    let mut next_poll = Instant::now();

    loop {
        // Commands first: a shutdown must not wait for a poll tick.
        loop {
            match commands.try_recv() {
                Ok(line) => match parse_command(&line) {
                    Ok(command) => {
                        let name = command.name();
                        eprintln!("freebuff-driver: {name} received");
                        session.handle_start(command);
                    }
                    Err(reason) => {
                        session.diag(&reason);
                    }
                },
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    session.diag("stdin closed; ending the Freebuff session");
                    session.finish(0);
                }
            }
        }

        session.pump_pty();
        session.update_settled();
        session.maybe_send_prompt();

        if Instant::now() >= next_poll {
            next_poll = Instant::now() + POLL;
            session.poll_chat();
            session.note_no_activity();
        }

        session.drive_interrupt();
        session.check_exit();

        if session.events.dead() {
            session.finish(1);
        }

        std::thread::sleep(Duration::from_millis(40));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bracketed_paste_wraps_in_escape_markers_without_cr() {
        let bytes = bracketed_paste("hello world");
        assert_eq!(bytes, b"\x1b[200~hello world\x1b[201~");
    }

    #[test]
    fn note_no_activity_detects_offline_screen() {
        let mut session = Session::new();
        session.turn_open = true;
        session.last_progress = Instant::now() - Duration::from_secs(60);
        session.screen = "Error: Can't reach Freebuff's servers. Try another network".to_string();
        session.note_no_activity();
        assert!(!session.turn_open);
    }
}
