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

use crate::catalog;
use crate::chatdir::{self, ChatDir};
use crate::keys::{self, Key};
use crate::logtail::{LogTail, TurnSignal};
use crate::picker;
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
/// A turn that has made no progress for this long is closed as an error.
///
/// The driver reads Freebuff's chat store, and that store is internal to the
/// CLI: it can move, or stop being written, without the CLI dying. When that
/// happens nothing else in the driver would ever end the turn, and the chat
/// would sit "working" for ever with nothing in the console to explain it.
/// Closing it as an error is the honest end: the turn's outcome is unknown,
/// and a person can retry instead of waiting on a spinner.
const TURN_LIMIT: Duration = Duration::from_secs(120);
/// How recently the TUI must have printed for its screen to count as alive.
/// A live screen earns three times the window (see `stall_window`), because a
/// TUI that is still drawing may be inside one long tool step that writes
/// nothing to the transcript.
const SCREEN_ALIVE: Duration = Duration::from_secs(15);
/// How often the screen is checked for Freebuff's own "cannot reach the
/// servers" wording. A second is often enough for a banner and cheap over the
/// few kilobytes the driver keeps.
const OFFLINE_CHECK: Duration = Duration::from_secs(1);
/// How many send confirmations the driver will press in one turn. Freebuff can
/// ask again after a model switch; an unbounded loop would keep spending.
const CONFIRM_LIMIT: u32 = 3;
/// Least time between two confirmations, so a repaint cannot look like a fresh
/// question and take a second press for the same message.
const CONFIRM_GAP: Duration = Duration::from_secs(3);
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

/// How long a turn may go without progress before it is closed as stalled.
/// `FREEBUFF_DRIVER_TURN_LIMIT_SECS` shortens it while an author is developing
/// a driver; it changes no event's shape, only how soon the end arrives.
fn turn_limit() -> Duration {
    std::env::var("FREEBUFF_DRIVER_TURN_LIMIT_SECS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .map(Duration::from_secs)
        .filter(|limit| *limit >= Duration::from_secs(5))
        .unwrap_or(TURN_LIMIT)
}

/// Bracketed paste: the TUI inserts the text.
fn bracketed_paste(text: &str) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(text.len() + 16);
    bytes.extend_from_slice(b"\x1b[200~");
    bytes.extend_from_slice(text.as_bytes());
    bytes.extend_from_slice(b"\x1b[201~");
    bytes
}

/// Whether a turn has gone quiet for long enough to be closed as stalled.
/// Kept as a plain function so the rule is testable without waiting minutes.
fn stalled(idle: Duration, limit: Duration) -> bool {
    idle >= limit
}

/// How long a turn may stay silent: a frozen screen gets the short window, a
/// screen that is still printing gets three times as long, because a live TUI
/// may be inside one long tool step that writes nothing to the transcript.
fn stall_window(screen_idle: Duration, frozen: Duration) -> Duration {
    if screen_idle >= SCREEN_ALIVE {
        frozen
    } else {
        frozen * 3
    }
}

/// How often a settle loop pumps the PTY while it waits. The TUI paints in
/// bursts, so a shorter step only wastes wake-ups, and a longer one could miss
/// the quiet window it is looking for.
const SETTLE_TICK: Duration = Duration::from_millis(25);

/// What a settle loop should do after looking at the screen and the clock.
/// Kept as a plain enum so the rule can be tested without a pseudo terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettleOutcome {
    /// The TUI has not printed for the quiet window: its screen is readable.
    Quiet,
    /// The limit was spent while the TUI was still printing. Read the screen
    /// anyway and decide from what is there; never assume it is ready.
    TimedOut,
    /// Keep pumping: the TUI printed recently and the limit is not spent.
    Waiting,
}

/// The settle rule: quiet wins as soon as the TUI has been silent long enough,
/// and otherwise the limit is the only way out.
pub fn settle_verdicts(
    last_output_elapsed: Duration,
    quiet: Duration,
    waited: Duration,
    limit: Duration,
) -> SettleOutcome {
    if last_output_elapsed >= quiet {
        SettleOutcome::Quiet
    } else if waited >= limit {
        SettleOutcome::TimedOut
    } else {
        SettleOutcome::Waiting
    }
}

/// How long the picker takes to answer one key, and the bound on it. A picker
/// repaints in one go, so this only has to cover that repaint, not a startup.
const MODEL_STEP_QUIET: Duration = Duration::from_millis(350);
const MODEL_STEP_LIMIT: Duration = Duration::from_secs(3);
/// The most Down presses one walk spends. The picker wraps, so a model away
/// from the current row is a few presses away; a name Freebuff no longer
/// offers is refused by the bound instead of pressing for ever.
const MODEL_MAX_PRESSES: u32 = 240;
/// How many presses in a row may leave the mark where it was. A list that
/// stopped moving is a list whose bottom has been reached.
const MODEL_STANDING_STILL: u32 = 2;
/// How many interaction frames a failed switch may quote.
const MODEL_FRAMES: usize = 8;

/// Whether Freebuff has to be pointed at `wanted` before the chat can use it.
///
/// `current` is the row the picker marked when it was read. Nothing read yet
/// means the picker has to be opened: the driver never assumes the chat woke
/// up on the model the console asked for.
pub fn needs_switch(current: Option<&str>, wanted: &str) -> bool {
    match current {
        Some(current) => current.trim() != wanted.trim(),
        None => true,
    }
}

/// The model `initialized` names: the one the picker showed when it was read,
/// or the requested name when the driver never got to look at the screen.
///
/// The console's chip then shows what the driver actually saw, so a chat that
/// opened on Freebuff's own model says so instead of echoing the request.
pub fn reported_model(verified: Option<&str>, requested: &str) -> String {
    verified.unwrap_or(requested).to_string()
}

/// What the arrow walk may do after one press.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WalkVerdict {
    /// The mark is somewhere else: press again.
    Continue,
    /// The mark is on the wanted row: select it.
    Found,
    /// Stop: the list closed, or the press budget is spent.
    GiveUp,
}

pub fn walk_verdict(
    current: Option<&str>,
    wanted: &str,
    presses: u32,
    picker_open: bool,
) -> WalkVerdict {
    // The list closed under the driver. Down belongs to the chat composer
    // once it has, and this driver does not own that composer.
    if !picker_open {
        return WalkVerdict::GiveUp;
    }
    if !needs_switch(current, wanted) {
        return WalkVerdict::Found;
    }
    if presses >= MODEL_MAX_PRESSES {
        return WalkVerdict::GiveUp;
    }
    WalkVerdict::Continue
}

/// What the picker opening may do next.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenVerdict {
    /// The screen is the picker: every keystroke below is a picker key.
    Ready,
    /// The screen is not the picker and the one retry is unspent.
    Retry,
    /// Give up: the retry is spent and the screen is still not the list.
    GiveUp,
}

/// Whether `/model` opened the picker, decided from the screen itself.
///
/// A startup notice, a survey or an ad can swallow the first `/model`, so the
/// driver may try once more; after that the screen is not the list and no
/// further keystroke is sent.
pub fn opening_verdict(tail: Option<&[String]>, attempts: u32) -> OpenVerdict {
    match catalog::read_tail(tail) {
        catalog::ScreenRead::Picker(_) => OpenVerdict::Ready,
        _ if attempts == 0 => OpenVerdict::Retry,
        _ => OpenVerdict::GiveUp,
    }
}

/// The name of the row the picker marks, when one is visible.
fn marked_name(picker: &picker::Picker) -> Option<String> {
    picker
        .current
        .and_then(|at| picker.cards.get(at))
        .map(|card| card.name.clone())
}

/// The model name a `spawn` asked for.
///
/// Freebuff takes a name from its own picker. Anything else in that field (an
/// object another harness uses, a number) is not a model this chat can start
/// on, so it counts as no choice at all rather than a guess.
fn wanted_model(model: Option<&Value>) -> Option<String> {
    match model? {
        Value::String(name) if !name.trim().is_empty() => Some(name.trim().to_string()),
        _ => None,
    }
}

/// Whether the TUI is showing Freebuff's own "I cannot reach my servers"
/// wording. Matching is case-insensitive because the CLI's own capitalisation
/// is not a contract, and these phrases are distinctive enough that assistant
/// prose is unlikely to contain them.
fn screen_says_offline(screen: &str) -> bool {
    let lower = screen.to_lowercase();
    [
        "can't reach freebuff's servers",
        "can\u{2019}t reach freebuff\u{2019}s servers",
        "try another network",
        "health check failed",
        "no internet connection",
        "check your network connection",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
}

/// Whether the TUI is holding a typed message behind its own send
/// confirmation.
///
/// Freebuff asks a second time before it spends the account's balance on a
/// model: the message sits in the composer with "Starting your model
/// session… Your message is saved." and "Enter: confirm and send". The single
/// Enter a send ends there, so without a second one the turn never starts and
/// the transcript stays empty - which looks exactly like a dead CLI.
fn screen_asks_to_confirm(screen: &str) -> bool {
    let lower = screen.to_lowercase();
    [
        "confirm and send",
        "enter: confirm",
        "press enter to confirm",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
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
    last_offline_check: Option<Instant>,
    /// Send confirmations the driver has pressed this turn. Freebuff may ask
    /// again after a model switch, but a loop here would spend the account's
    /// balance, so the count is bounded.
    confirmations: u32,
    last_confirm_at: Option<Instant>,
    /// The screen a person sees. Matchers read this, never the stripped text.
    screen: crate::screen::Screen,
    /// Stripped text, for diagnostics only: a frame can carry an account string.
    frame_text: String,
    /// The model switch that failed, if one did: (requested, actual when the
    /// picker was read). While this is set, no prompt may be typed.
    model_error: Option<(String, Option<String>)>,
    /// The model the picker itself showed, which is what `initialized` names.
    model_verified: Option<String>,
    /// The last few interaction frames. Quoted in a diagnostic when a switch
    /// fails, and nowhere else: a frame can carry an account string.
    frames: crate::screen::FrameLog,
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
            last_offline_check: None,
            confirmations: 0,
            last_confirm_at: None,
            screen: crate::screen::Screen::new(crate::ptyproc::ROWS, crate::ptyproc::COLS),
            frame_text: String::new(),
            model_error: None,
            model_verified: None,
            frames: crate::screen::FrameLog::new(MODEL_FRAMES),
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

        // The CLI, its version and the sign-in are the same three checks the
        // catalog capture makes, so they live in one place.
        let preflight = match probe::preflight() {
            Ok(preflight) => preflight,
            Err(reason) => self.fail_closed(&reason),
        };
        self.diag(&format!(
            "Freebuff {} at {}",
            preflight.version,
            preflight.exe.display()
        ));

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

        let pty = match PtyProc::spawn(&preflight.exe, &args, &cwd_path) {
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

        if let Some(raw) = self.model.as_ref() {
            match wanted_model(Some(raw)) {
                Some(wanted) => self.choose_starting_model(&wanted),
                None => self.diag(
                    "the spawn's model is not a Freebuff model name; this chat starts on the \
                     model Freebuff already runs",
                ),
            }
        }
    }

    /// Put the chat on `wanted` before any prompt may be typed.
    ///
    /// A switch that fails does not end the session: the chat stays usable,
    /// `initialized` names the model it really runs, and the prompt is refused
    /// in a sentence until the person picks again. That refusal is what keeps
    /// a spend off a model nobody chose.
    fn choose_starting_model(&mut self, wanted: &str) {
        self.diag(&format!(
            "reading Freebuff's model picker to start this chat on {wanted}"
        ));
        self.wait_for_first_paint();
        match self.select_model(wanted) {
            Ok(()) => {
                let verified = self
                    .model_verified
                    .clone()
                    .unwrap_or_else(|| wanted.to_string());
                self.diag(&format!("Freebuff runs {verified}"));
            }
            Err(reason) => self.diag(&format!("model: {reason}")),
        }
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
        // Sending here would spend on a model the user did not choose, and
        // dropping the prompt silently would leave the console waiting on a
        // turn that never starts. A reported refusal is the honest third path.
        if let Some(block) = self.prompt_block() {
            self.diag(&block);
            let _ = self
                .events
                .turn_completed("error", "failed", true, Some(&block));
            return;
        }
        self.pending_prompt = Some(text);
    }

    /// The sentence a prompt is refused with when the chat could not be put on
    /// the model the console picked.
    ///
    /// `None` means the prompt may be typed. `Some` names the requested model
    /// by its own words and, when the picker was read, the model the chat runs
    /// instead, so a person can decide without opening the picker themselves.
    pub fn prompt_block(&self) -> Option<String> {
        let (requested, actual) = self.model_error.as_ref()?;
        Some(match actual {
            Some(actual) => format!(
                "Freebuff did not switch to {requested}: it is still on {actual}, \
                 so the prompt was not sent. Pick another model, then send it again."
            ),
            None => format!(
                "Freebuff did not switch to {requested}, so the prompt was not sent. \
                 Pick another model, then send it again."
            ),
        })
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
            pty.send(&keys::bytes(Key::Escape));
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
            pty.send(&keys::bytes(Key::Enter));
        }
        self.prompt_count += 1;
        self.turn_open = true;
        self.last_progress = Instant::now();
        self.last_activity_note = None;
        // A fresh turn gets its own confirmation budget.
        self.confirmations = 0;
        self.last_confirm_at = None;
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
        // Two consumers, two views: the grid is what a matcher may read (the
        // screen a person sees), the stripped text is only ever quoted in a
        // diagnostic.
        self.screen.feed(&bytes);
        self.frame_text
            .push_str(&crate::ptyproc::strip_ansi(&bytes));
        if self.frame_text.len() > 8 * SCREEN_TAIL {
            let cut = self.frame_text.len() - 4 * SCREEN_TAIL;
            let mut start = cut;
            while start < self.frame_text.len() && !self.frame_text.is_char_boundary(start) {
                start += 1;
            }
            self.frame_text = self.frame_text[start..].to_string();
        }
        if self.interrupting.is_some() {
            // Any repaint counts as the TUI still working on the cancel.
            self.last_progress = Instant::now();
        }
    }

    /// Pump the PTY until the TUI has stopped printing for `quiet`, or until
    /// `limit` has been spent. Returns true when it went quiet and false when
    /// the limit ran out; callers are expected to read the screen either way,
    /// because a timed-out settle means "look at what is there", never "assume
    /// the TUI is ready".
    ///
    /// The clock is the TUI's own last output, so a screen that was already
    /// quiet settles at once. A caller that has just pressed a key and needs
    /// the repaint before reading should wait for output another way.
    pub fn settle(&mut self, quiet: Duration, limit: Duration) -> bool {
        let started = Instant::now();
        loop {
            self.pump_pty();
            match settle_verdicts(self.last_output.elapsed(), quiet, started.elapsed(), limit) {
                SettleOutcome::Quiet => return true,
                SettleOutcome::TimedOut => return false,
                SettleOutcome::Waiting => std::thread::sleep(SETTLE_TICK),
            }
        }
    }

    /// Pump until the TUI prints something newer than `before`, or the bound
    /// is up.
    fn wait_for_fresh_output(&mut self, before: Instant) -> bool {
        let started = Instant::now();
        loop {
            self.pump_pty();
            if self.last_output > before {
                return true;
            }
            // A CLI that closed its output will never repaint, so there is
            // nothing to wait for and nothing a keystroke could reach.
            if self.pty.as_ref().map(PtyProc::eof).unwrap_or(true) {
                return false;
            }
            if started.elapsed() >= MODEL_STEP_LIMIT {
                return false;
            }
            std::thread::sleep(SETTLE_TICK);
        }
    }

    /// Press one key and read the screen Freebuff drew in answer to it.
    ///
    /// `settle` on its own is not enough after a key: its clock counts from
    /// the TUI's last output, which at the moment of the press is the screen
    /// *before* the keystroke, so it can return before the repaint and the
    /// parse that follows would read the pre-key screen. Waiting for fresh
    /// output first is what makes the screen after a key the screen a person
    /// would see. Every key the model walk sends passes through here, and
    /// every one leaves a frame a failed switch can be diagnosed from.
    fn press_and_settle(&mut self, label: &str, key: Key) {
        let before = self.last_output;
        if let Some(pty) = self.pty.as_ref() {
            pty.send(&keys::bytes(key));
        }
        // Both waits ignore their own answer on purpose: the parse that
        // follows decides what the screen is, and a timed-out wait means
        // "read it and see", never "assume it is ready".
        let _ = self.wait_for_fresh_output(before);
        let _ = self.settle(MODEL_STEP_QUIET, MODEL_STEP_LIMIT);
        let tail = self.screen_tail();
        self.frames.push(label, tail);
    }

    /// The rows a person can see, or nothing when the grid could not follow
    /// the last repaint.
    fn screen_rows(&self) -> Option<Vec<String>> {
        let (rows, _cols) = self.screen.size();
        self.screen.tail(rows as usize)
    }

    /// The picker as the screen shows it, if it shows one.
    fn read_picker(&self) -> Option<picker::Picker> {
        picker::parse(self.screen_rows()?.as_slice())
    }

    /// Wait for the startup paint and the quiet after it before typing.
    ///
    /// The session's settle rule, with one addition: a screen has to have
    /// painted at least once. The quiet clock starts at spawn, so a TUI that
    /// has drawn nothing is not quiet, it has not started, and typing into it
    /// is how a key ends up in Freebuff's startup output.
    fn wait_for_first_paint(&mut self) {
        let started = Instant::now();
        loop {
            self.pump_pty();
            match settle_verdicts(
                self.last_output.elapsed(),
                SETTLE_QUIET,
                started.elapsed(),
                SETTLE_LIMIT,
            ) {
                SettleOutcome::Quiet if self.seen_output => return,
                SettleOutcome::TimedOut => return,
                SettleOutcome::Quiet | SettleOutcome::Waiting => {
                    std::thread::sleep(SETTLE_TICK);
                }
            }
        }
    }

    /// Open the model picker and return what it shows.
    ///
    /// A startup notice, a survey or an ad can swallow the first `/model`, so
    /// the attempt is made twice. The retry closes whatever is on screen and
    /// only types again on a screen known to be readable and known not to be
    /// the picker: Enter on an open list would select the highlighted row,
    /// which is the one keystroke here that could change the user's model.
    fn open_model_picker(&mut self) -> Result<picker::Picker, String> {
        for attempt in 0..2 {
            if attempt > 0 {
                self.press_and_settle("model: escape", Key::Escape);
                catalog::typing_is_safe(&self.screen)?;
            }
            self.press_and_settle("model: /model", Key::Text("/model".to_string()));
            self.press_and_settle("model: enter", Key::Enter);
            let tail = self.screen_rows();
            match opening_verdict(tail.as_deref(), attempt) {
                OpenVerdict::Ready => {
                    if let Some(picker) = tail.as_deref().and_then(picker::parse) {
                        self.diag(&format!(
                            "the model picker is open with {} card(s)",
                            picker.cards.len()
                        ));
                        return Ok(picker);
                    }
                    return Err("Freebuff's model picker could not be read".to_string());
                }
                OpenVerdict::Retry => continue,
                OpenVerdict::GiveUp => {
                    return Err("Freebuff did not open its model picker".to_string());
                }
            }
        }
        Err("Freebuff did not open its model picker".to_string())
    }

    /// Put this chat on `wanted`, and prove it with the screen.
    ///
    /// Nothing here types a prompt or answers a spend dialog: the keys are
    /// `/model`, Enter, Down and Escape. A switch that cannot be verified is
    /// an error, so the caller refuses the prompt instead of spending on a
    /// model the user did not choose.
    pub fn select_model(&mut self, wanted: &str) -> Result<(), String> {
        let wanted = wanted.trim().to_string();
        if self.pty.is_none() {
            return Err(self.refuse_switch(&wanted, None, "Freebuff is not running".to_string()));
        }
        // A new attempt supersedes the last refusal: only a switch that fails
        // again keeps the prompt blocked, so a chat that was put right is not
        // stuck behind a stale sentence.
        self.model_error = None;

        // 1. Read the row the picker marks: that is the model the chat runs
        // now, and the only reading taken before an arrow is pressed.
        let first = match self.open_model_picker() {
            Ok(picker) => picker,
            Err(reason) => return Err(self.refuse_switch(&wanted, None, reason)),
        };
        let mut marked = marked_name(&first);
        self.model_verified = marked.clone();

        // 2. Freebuff already runs it: close the list, press no arrow, and take
        // the catalog refresh this screen hands over for free.
        if !needs_switch(marked.as_deref(), &wanted) {
            self.press_and_settle("model: escape", Key::Escape);
            catalog::write_models(&first);
            return Ok(());
        }
        self.diag(&format!(
            "Freebuff runs {}, so the picker is walked to {wanted}",
            marked
                .as_deref()
                .unwrap_or("a model the screen does not name")
        ));

        // 3. Walk the list. The picker wraps, so Down reaches every row from
        // the current one, and typing is never used to filter it.
        let mut presses = 0u32;
        let mut standing = 0u32;
        loop {
            self.press_and_settle(&format!("model: down {presses}"), Key::Down);
            presses += 1;
            let next = self.read_picker();
            let next_marked = next.as_ref().and_then(marked_name);
            let verdict = walk_verdict(next_marked.as_deref(), &wanted, presses, next.is_some());
            if verdict == WalkVerdict::Found {
                marked = next_marked;
                break;
            }
            if verdict == WalkVerdict::GiveUp {
                return Err(self.refuse_switch(
                    &wanted,
                    marked.clone(),
                    format!("Freebuff does not offer a model called {wanted}"),
                ));
            }
            if next_marked == marked {
                standing += 1;
                if standing >= MODEL_STANDING_STILL {
                    // Down is not moving the mark any more, so the list is at
                    // its bottom and the wanted name was not in it.
                    return Err(self.refuse_switch(
                        &wanted,
                        marked.clone(),
                        format!("Freebuff does not offer a model called {wanted}"),
                    ));
                }
            } else {
                standing = 0;
                marked = next_marked;
            }
        }
        self.diag(&format!(
            "the picker marks {wanted} after {presses} press(es); selecting it"
        ));

        // 4. Enter selects the highlighted row. A list still on screen means
        // the key did not take, and Escape closes it before another key.
        self.press_and_settle("model: enter", Key::Enter);
        if self.read_picker().is_some() {
            self.press_and_settle("model: escape", Key::Escape);
            return Err(self.refuse_switch(
                &wanted,
                marked.clone(),
                "Freebuff did not close its picker".to_string(),
            ));
        }

        // 5. Verify the only way this screen allows: open the picker again and
        // read which row it marks. Nothing else proves the switch was taken.
        let reopened = match self.open_model_picker() {
            Ok(picker) => picker,
            Err(reason) => return Err(self.refuse_switch(&wanted, marked.clone(), reason)),
        };
        let verified = marked_name(&reopened);
        self.press_and_settle("model: escape", Key::Escape);
        if verified.as_deref().map(str::trim) != Some(wanted.as_str()) {
            let actual = verified.clone().or_else(|| marked.clone());
            return Err(self.refuse_switch(
                &wanted,
                actual.clone(),
                format!(
                    "Freebuff kept {} after the switch to {wanted}",
                    actual.as_deref().unwrap_or("its own model")
                ),
            ));
        }
        self.model_verified = verified;
        // The screen that proved the switch is also a catalog read.
        catalog::write_models(&reopened);
        Ok(())
    }

    /// Record a failed switch and hand the caller its sentence.
    ///
    /// The last frame is quoted on stderr only: it is the one thing that says
    /// what the screen looked like when the walk gave up, and a frame can
    /// carry the signed-in address or a balance, so it never becomes an event
    /// field a synced chat would keep.
    fn refuse_switch(&mut self, wanted: &str, actual: Option<String>, reason: String) -> String {
        self.model_error = Some((wanted.trim().to_string(), actual));
        let last = self
            .frames
            .last()
            .map(|(label, tail)| (label.to_string(), tail.to_string()));
        if let Some((label, tail)) = last {
            self.diag(&format!("model: after {label} the screen showed: {tail}"));
        }
        reason
    }

    /// The model `initialized` names.
    ///
    /// With a requested model, the name is the one the driver proved, or the
    /// requested name when the picker was never read. With no request at all
    /// the driver has nothing to add, so the daemon's own value stands.
    fn reported(&self) -> Option<Value> {
        match wanted_model(self.model.as_ref()) {
            Some(requested) => Some(Value::String(reported_model(
                self.model_verified.as_deref(),
                &requested,
            ))),
            None => self.model.clone(),
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
            // The chip shows the model the driver saw, never the request.
            let model = self.reported();
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

    /// What the screen says about the turn, checked about once a second: an
    /// unreachable-server banner ends it, and Freebuff's send confirmation gets
    /// the second Enter it is waiting for.
    fn scan_screen(&mut self) {
        if !self.turn_open || self.interrupting.is_some() {
            return;
        }
        if let Some(at) = self.last_offline_check {
            if at.elapsed() < OFFLINE_CHECK {
                return;
            }
        }
        self.last_offline_check = Some(Instant::now());
        if self.screen.tail(1).is_none() {
            // The grid could not follow the last repaint: nothing here is
            // something a person is looking at, so no matcher may read it.
            return;
        }

        // The tail, not the whole screen: rows from an older frame must not
        // match, or a dismissed dialog would be answered again - and a
        // confirmation pressed again and again spends the account's balance.
        let tail = self.screen_tail();

        if screen_says_offline(&tail) {
            self.end_turn(Some(
                "Can't reach Freebuff's servers. Retrying… Try another network.".to_string(),
            ));
            return;
        }

        // The message the user already sent is waiting on Freebuff's own
        // cost confirmation. Sending it was their choice; the extra Enter is
        // the same key they would press, not a decision the driver invents.
        if screen_asks_to_confirm(&tail) && self.confirmations < CONFIRM_LIMIT {
            match self.last_confirm_at {
                Some(at) if at.elapsed() < CONFIRM_GAP => return,
                _ => {}
            }
            self.confirmations += 1;
            self.last_confirm_at = Some(Instant::now());
            self.diag(
                "Freebuff is asking to confirm the send; pressing Enter, as the message \
                 was already sent",
            );
            if let Some(pty) = self.pty.as_ref() {
                pty.send(&keys::bytes(Key::Enter));
            }
        }
    }

    fn note_no_activity(&mut self) {
        if !self.turn_open || self.interrupting.is_some() {
            return;
        }
        let limit = activity_note_limit();
        if self.last_progress.elapsed() < limit {
            return;
        }

        // Nothing has arrived for the whole turn window: the store this driver
        // reads is not being written for this chat any more. End it as an
        // error rather than leave the chat waiting on a spinner for ever.
        let window = stall_window(self.last_output.elapsed(), turn_limit());
        if stalled(self.last_progress.elapsed(), window) {
            // The screen quote stays a diagnostic: `error_message` is held to
            // the redacted, clipped shape this protocol promises, and a TUI
            // frame can carry an account string.
            let screen = self.screen_tail();
            if !screen.is_empty() {
                self.diag(&format!("the TUI is showing: {screen}"));
            }
            self.end_turn(Some(format!(
                "Freebuff reported nothing for {} seconds; the turn was closed as stalled",
                window.as_secs()
            )));
            return;
        }

        if let Some(at) = self.last_activity_note {
            if at.elapsed() < limit {
                return;
            }
        }
        self.last_activity_note = Some(Instant::now());

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
    /// better than answering it blindly. A screen this grid could not fully
    /// follow quotes nothing: a wrong frame is worse than an empty one.
    fn screen_tail(&self) -> String {
        let Some(mut text) = self.screen.joined_tail(12) else {
            return String::new();
        };
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
        // Independent of the poll tick: a "cannot reach the servers" banner
        // should be noticed in about a second, not on the slower poll of the
        // chat store.
        session.scan_screen();

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
    fn the_offline_wording_is_recognized_whatever_its_case() {
        for screen in [
            "Can't reach Freebuff's servers",
            "can't reach Freebuff's servers. Retrying…",
            "CAN’T REACH FREEBUFF’S SERVERS",
            "Try another network",
            "health check failed, backing off",
            "No internet connection",
            "please check your network connection",
        ] {
            assert!(screen_says_offline(screen), "{screen}");
        }
        // Ordinary conversation and chrome are not that banner.
        for screen in [
            "Help me write a function that retries a request",
            "The build finished in 12 seconds",
            "thinking...",
            "",
        ] {
            assert!(!screen_says_offline(screen), "{screen}");
        }
    }

    #[test]
    fn the_offline_banner_ends_a_turn_within_a_second_not_a_minute() {
        let mut session = Session::new();
        session.turn_open = true;
        // No transcript progress at all: only the screen check can end this.
        session.last_progress = Instant::now();
        session
            .screen
            .feed("Can't reach Freebuff's servers. Retrying…".as_bytes());

        session.scan_screen();

        assert!(!session.turn_open, "the banner must end the turn");
    }

    #[test]
    fn a_quiet_screen_is_not_checked_again_within_the_second() {
        let mut session = Session::new();
        session.turn_open = true;
        session.last_progress = Instant::now();
        session.last_offline_check = Some(Instant::now());
        session
            .screen
            .feed("Can't reach Freebuff's servers".as_bytes());

        session.scan_screen();

        assert!(session.turn_open, "one check a second is enough");
    }

    #[test]
    fn the_send_confirmation_is_recognized() {
        // Verbatim from the TUI that stalled a real turn.
        let stalled = "│  reply with exactly DIAG▍│ │ Starting your model session… Your message is saved.│ │ Esc: back to draft│ DeepSeek V4.1 Flash 10/17 takes 3 hours from your balance. Enter: confirm and send  Esc: back to draft";
        assert!(screen_asks_to_confirm(stalled));
        assert!(screen_asks_to_confirm("Enter: confirm and send"));
        assert!(screen_asks_to_confirm("press enter to confirm"));
        // A normal composer is not a confirmation.
        assert!(!screen_asks_to_confirm("│  reply with exactly DIAG▍│"));
        assert!(!screen_asks_to_confirm("confirm this file was written"));
    }

    #[test]
    fn a_waiting_message_gets_the_second_enter() {
        let mut session = Session::new();
        session.turn_open = true;
        session.last_progress = Instant::now();
        session.screen.feed(
            "Starting your model session… Enter: confirm and send  Esc: back to draft".as_bytes(),
        );

        session.scan_screen();

        assert_eq!(
            session.confirmations, 1,
            "the driver confirms a message the user already sent"
        );
        assert!(session.turn_open, "confirming does not end the turn");
    }

    #[test]
    fn the_confirmation_is_bounded_and_paced() {
        let mut session = Session::new();
        session.turn_open = true;
        session.screen.feed("Enter: confirm and send".as_bytes());

        // Two presses inside the pacing gap are one press.
        session.scan_screen();
        session.last_offline_check = None;
        session.scan_screen();
        assert_eq!(session.confirmations, 1, "a repaint is not a new question");

        // Even given all the time in the world, the driver stops pressing.
        for _ in 0..CONFIRM_LIMIT + 2 {
            session.last_offline_check = None;
            session.last_confirm_at = Some(Instant::now() - CONFIRM_GAP - Duration::from_secs(1));
            session.scan_screen();
        }
        assert_eq!(
            session.confirmations, CONFIRM_LIMIT,
            "a confirmation loop must not spend without limit"
        );
    }

    #[test]
    fn a_dialog_already_dismissed_does_not_press_again() {
        let mut session = Session::new();
        session.turn_open = true;
        // The dialog is at the top of the screen, and the TUI has since drawn
        // twenty lines over it, so it is out of the tail a person reads.
        session
            .screen
            .feed(b"Enter: confirm and send  Esc: back to draft\n");
        for i in 0..20 {
            session
                .screen
                .feed(format!("line {i} of the model talking\n").as_bytes());
        }

        session.scan_screen();

        assert_eq!(
            session.confirmations, 0,
            "only the screen a person can see may be answered"
        );
    }

    #[test]
    fn a_turn_that_never_progresses_is_closed_instead_of_hanging() {
        // The store this driver reads can stop being written without the CLI
        // dying. Before the watchdog such a turn stayed open for ever, so the
        // console showed a spinner and nothing else.
        let frozen = Duration::from_secs(120);
        assert!(!stalled(Duration::from_secs(30), frozen));
        assert!(stalled(Duration::from_secs(120), frozen));
        assert!(stalled(Duration::from_secs(600), frozen));
    }

    #[test]
    fn a_screen_that_is_still_printing_earns_a_longer_window() {
        let frozen = Duration::from_secs(120);
        // The TUI has drawn within the last few seconds: keep waiting longer,
        // because a tool step can run for minutes without writing anything.
        assert_eq!(stall_window(Duration::from_secs(2), frozen), frozen * 3);
        // Nothing drawn for a while: the short window applies.
        assert_eq!(stall_window(Duration::from_secs(60), frozen), frozen);
        assert_eq!(stall_window(Duration::from_secs(15), frozen), frozen);
    }

    #[test]
    fn a_stalled_turn_reports_an_error_and_clears_the_turn() {
        let mut session = Session::new();
        session.turn_open = true;
        session.last_progress = Instant::now() - turn_limit() - Duration::from_secs(1);
        // A frozen screen, which is the case the short window is for.
        session.last_output = Instant::now() - Duration::from_secs(60);
        session.screen.feed("some TUI text".as_bytes());

        session.note_no_activity();

        assert!(!session.turn_open, "a stalled turn must not stay open");
        assert!(session.interrupting.is_none());
    }

    #[test]
    fn a_live_screen_delays_the_stall_end() {
        let mut session = Session::new();
        session.turn_open = true;
        session.last_progress = Instant::now() - turn_limit() - Duration::from_secs(1);
        // The TUI is still drawing, so the turn gets the longer window.
        session.last_output = Instant::now();
        session.screen.feed("still working".as_bytes());

        session.note_no_activity();

        assert!(
            session.turn_open,
            "a TUI that is still printing may be inside a long tool step"
        );
    }

    #[test]
    fn a_turn_with_progress_is_left_alone() {
        let mut session = Session::new();
        session.turn_open = true;
        session.last_progress = Instant::now();

        session.note_no_activity();

        assert!(session.turn_open, "a fresh turn must not be closed");
    }

    #[test]
    fn settle_reports_quiet_once_the_tui_stopped_printing() {
        use SettleOutcome::*;
        assert_eq!(
            settle_verdicts(
                Duration::from_millis(400),
                Duration::from_millis(350),
                Duration::from_secs(1),
                Duration::from_secs(3)
            ),
            Quiet
        );
    }

    #[test]
    fn settle_gives_up_at_the_limit_instead_of_waiting_for_ever() {
        use SettleOutcome::*;
        assert_eq!(
            settle_verdicts(
                Duration::from_millis(50),
                Duration::from_millis(350),
                Duration::from_secs(3),
                Duration::from_secs(3)
            ),
            TimedOut
        );
    }

    #[test]
    fn settle_waits_while_the_tui_is_still_printing() {
        use SettleOutcome::*;
        assert_eq!(
            settle_verdicts(
                Duration::from_millis(50),
                Duration::from_millis(350),
                Duration::from_secs(1),
                Duration::from_secs(3)
            ),
            Waiting
        );
    }

    #[test]
    fn no_arrow_is_sent_when_freebuff_already_runs_the_wanted_model() {
        assert!(!needs_switch(
            Some("DeepSeek V4.1 Flash 10/17 • high"),
            "DeepSeek V4.1 Flash 10/17 • high"
        ));
        assert!(needs_switch(
            Some("DeepSeek V4.1 Flash Slow • high"),
            "DeepSeek V4.1 Flash 10/17 • high"
        ));
        // Nothing read yet: read the picker, never assume.
        assert!(needs_switch(None, "DeepSeek V4.1 Flash 10/17 • high"));
    }

    #[test]
    fn a_prompt_after_a_failed_switch_is_refused_not_sent() {
        let mut session = Session::new();
        session.model_error = Some(("X".into(), Some("Y".into())));
        let block = session.prompt_block().expect("a refusal");
        assert!(block.contains("did not switch to X"));
        assert!(block.contains("still on Y"));
        assert!(block.contains("not sent"));
    }

    #[test]
    fn an_unreadable_picker_refuses_rather_than_guesses() {
        let mut session = Session::new();
        session.model_error = Some(("X".into(), None));
        assert!(session
            .prompt_block()
            .unwrap()
            .contains("did not switch to X"));
    }

    #[test]
    fn the_walk_gives_up_instead_of_pressing_for_ever() {
        use WalkVerdict::*;
        assert_eq!(walk_verdict(Some("A"), "B", 0, true), Continue);
        assert_eq!(walk_verdict(Some("B"), "B", 7, true), Found);
        assert_eq!(walk_verdict(Some("A"), "B", 240, true), GiveUp);
        // The picker closed under us: no more presses, the screen is not the picker.
        assert_eq!(walk_verdict(Some("A"), "B", 3, false), GiveUp);
    }

    #[test]
    fn a_survey_between_keys_gets_one_retry_and_never_a_typed_prompt() {
        use OpenVerdict::*;
        let survey: Vec<String> = [
            "How did you hear about Freebuff?",
            "1. A friend",
            "2. Search",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        assert_eq!(opening_verdict(Some(&survey), 0), Retry);
        assert_eq!(opening_verdict(Some(&survey), 1), GiveUp);
        assert_eq!(opening_verdict(None, 0), Retry);
    }

    #[test]
    fn initialized_reports_the_verified_model_not_the_requested_one() {
        assert_eq!(reported_model(Some("Y"), "X"), "Y");
        assert_eq!(reported_model(None, "X"), "X");
    }
}
