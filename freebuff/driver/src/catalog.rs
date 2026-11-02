//! Reading Freebuff's model catalog off its own picker.
//!
//! `--catalog` is the one mode that opens the picker only to read it: it
//! starts a throwaway chat in a stable scratch folder, types `/model`, walks
//! the list with the arrow keys, and writes what the cards say to
//! `models.json` next to the driver. The console then shows Freebuff's real
//! list instead of a hand-copied one, and a release that adds or drops a model
//! is one refresh away.
//!
//! Nothing here can spend the account's balance: the only keys sent are
//! `/model`, one Enter that opens the picker, the arrow keys, and Escape. No
//! prompt is typed and no model is ever selected.
//!
//! The output is one JSON line whatever happens: an unavailable CLI, a screen
//! that is not the picker, or a TUI that exits are all answers the console can
//! show. Screen text never goes into that line, because it can carry the
//! signed-in address and a balance figure.

use std::io::Write;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::keys::{self, Key};
use crate::picker;
use crate::probe;
use crate::ptyproc::{self, PtyProc};
use crate::screen::Screen;
use crate::session::{settle_verdicts, SettleOutcome};

/// The catalog the console reads, written into the driver's own folder.
const MODELS_FILE: &str = "models.json";
/// Where the throwaway chat lands: one folder inside the plugin, so repeated
/// captures reuse the same Freebuff project instead of leaving a trail.
const SCRATCH_DIR: &str = ".catalog-cwd";
/// The wait before the first keystroke: the same paint-and-quiet rule the
/// session uses, because it is the same TUI being typed into.
const START_QUIET: Duration = Duration::from_millis(800);
const START_LIMIT: Duration = Duration::from_secs(8);
/// The wait after a keystroke. A picker repaints in one go, so this only has
/// to cover that repaint, not a whole startup.
const STEP_QUIET: Duration = Duration::from_millis(350);
const STEP_LIMIT: Duration = Duration::from_secs(3);
/// How often the wait loop looks at the terminal, as in the session.
const TICK: Duration = Duration::from_millis(25);
/// The most arrow presses one capture spends. Arrows cost nothing, but a
/// picker that keeps moving must not keep the driver here for ever.
const MAX_PRESSES: u32 = 240;
/// How many presses in a row may leave the mark where it was. A picker at the
/// end of its list ignores Down, and that is the signal that the list has been
/// seen to the bottom.
const STANDING_STILL: u32 = 2;

/// One model row, in the shape the daemon's `ModelDescriptor` has: the same
/// field names, so `models.json` parses as the daemon's own catalog. The
/// daemon's `resolved_model` and `reasoning_efforts` are left out because
/// Freebuff has neither, and every one of those fields defaults to empty.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ModelDescriptor {
    /// The name the picker prints, and therefore the name a `spawn` may ask
    /// for: the walk matches this string exactly.
    pub model: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    /// The picker's own cost line under the name (`3x · Recommended`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// The data-use notice the picker prints under a card.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// Whether the picker marked this row: the model Freebuff runs now.
    pub is_default: bool,
}

/// What `--catalog` prints and writes: the picker's cards, and when they were
/// read. The stamp is what makes a stale list visible instead of silent.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Catalog {
    /// Milliseconds since the Unix epoch, as the daemon stores it.
    pub captured_at: u64,
    /// The model the picker marked, when it marked one.
    pub default: Option<String>,
    pub models: Vec<ModelDescriptor>,
}

impl Catalog {
    /// Turn one reading of the picker into a catalog. The marked row becomes
    /// `default`, and each cost line travels beside the model it belongs to.
    pub fn from_picker(picker: &picker::Picker, captured_at: u64) -> Catalog {
        let default = picker
            .current
            .and_then(|at| picker.cards.get(at))
            .map(|card| card.name.clone());
        let models = picker
            .cards
            .iter()
            .enumerate()
            .map(|(at, card)| ModelDescriptor {
                model: card.name.clone(),
                display_name: Some(card.name.clone()),
                // A cost line can be cut off the top of the screen while the
                // name survives. The tagline is the picker's own words about
                // the same model, so the row keeps its second line either way.
                detail: card.detail.clone().or_else(|| card.tagline.clone()),
                note: card.note.clone(),
                is_default: picker.current == Some(at),
            })
            .collect();
        Catalog {
            captured_at,
            default,
            models,
        }
    }

    /// The one JSON line the driver prints and the daemon reads.
    pub fn to_line(&self) -> String {
        serde_json::to_string(self).unwrap_or_default()
    }
}

/// Read the model picker off `screen`, or `None` when the screen is not one.
///
/// This is the single place the capture asks "is the picker open", so a future
/// screen change breaks one named call instead of four inline parses.
pub fn capture_is_a_picker(screen: &Screen) -> Option<picker::Picker> {
    let (rows, _cols) = screen.size();
    picker::parse(&screen.tail(rows as usize)?)
}

/// The `--catalog` mode: one JSON line on stdout, always exit 0.
pub fn run() -> i32 {
    let line = match capture() {
        Ok(catalog) => {
            let line = catalog.to_line();
            // Best effort by design: the daemon falls back to the manifest's
            // model list when this file is missing, so a write that fails is
            // not worth failing the answer over.
            let _ = std::fs::write(MODELS_FILE, &line);
            line
        }
        Err(reason) => failure_line(&reason),
    };
    let mut stdout = std::io::stdout();
    let _ = writeln!(stdout, "{line}");
    let _ = stdout.flush();
    0
}

/// The one line an unavailable or unreadable CLI produces. `reason` is a
/// sentence this module wrote, never screen text: the daemon shows it in the
/// console, and a frame can carry an account string.
fn failure_line(reason: &str) -> String {
    serde_json::json!({
        "models": [],
        "default": serde_json::Value::Null,
        "captured_at": 0,
        "reason": reason,
    })
    .to_string()
}

/// Start a throwaway chat, read the whole picker, and close it again.
fn capture() -> Result<Catalog, String> {
    let preflight = probe::preflight()?;
    eprintln!(
        "freebuff-driver: catalog: Freebuff {} at {}",
        preflight.version,
        preflight.exe.display()
    );

    let cwd = std::env::current_dir()
        .map_err(|e| format!("could not find the driver's own folder: {e}"))?
        .join(SCRATCH_DIR);
    std::fs::create_dir_all(&cwd)
        .map_err(|e| format!("could not create {}: {e}", cwd.display()))?;

    let args = vec!["--cwd".to_string(), cwd.display().to_string()];
    let mut pty = PtyProc::spawn(&preflight.exe, &args, &cwd)?;
    let mut screen = Screen::new(ptyproc::ROWS, ptyproc::COLS);

    let read = read_picker(&mut pty, &mut screen);

    // Close the picker before the CLI dies under it, whether the read worked
    // or not: a reopened chat should not find a half-open list, and Escape in
    // a composer costs nothing.
    pty.send(&keys::bytes(Key::Escape));
    let _ = wait(&pty, &mut screen, STEP_QUIET, STEP_LIMIT);
    pty.kill();

    let picker = read?;
    let catalog = Catalog::from_picker(&picker, now_millis());
    eprintln!(
        "freebuff-driver: catalog: {} model(s) read{}",
        catalog.models.len(),
        catalog
            .default
            .as_ref()
            .map(|name| format!(", current one marked: {name}"))
            .unwrap_or_default()
    );
    Ok(catalog)
}

/// Open the picker, then page through it. Returns the merged reading.
fn read_picker(pty: &mut PtyProc, screen: &mut Screen) -> Result<picker::Picker, String> {
    // Typing before the first paint is how a keystroke ends up in Freebuff's
    // startup output instead of its composer.
    wait(pty, screen, START_QUIET, START_LIMIT)?;
    let first = open_picker(pty, screen)?;

    // Which row carries the mark before any arrow is pressed: that is the
    // model Freebuff runs now. After the first Down the mark is the driver's
    // cursor, so it must be read here and nowhere later.
    let default = first
        .current
        .and_then(|at| first.cards.get(at))
        .map(|card| card.name.clone());

    let mut cards: Vec<picker::Card> = Vec::new();
    merge(&mut cards, &first);

    let mut marked = default.clone();
    let mut marks_seen: Vec<String> = marked.iter().cloned().collect();
    let mut standing = 0u32;
    let mut presses = 0u32;

    while presses < MAX_PRESSES {
        pty.send(&keys::bytes(Key::Down));
        wait(pty, screen, STEP_QUIET, STEP_LIMIT)?;
        presses += 1;

        let Some(next) = capture_is_a_picker(screen) else {
            // The list closed under the driver. What was read is still true,
            // and pressing arrows into a chat composer helps nobody.
            break;
        };
        merge(&mut cards, &next);

        let Some(name) = next
            .current
            .and_then(|at| next.cards.get(at))
            .map(|card| card.name.clone())
        else {
            // No marked row to page by: nothing below can move the window.
            break;
        };
        if marked.as_ref() == Some(&name) {
            standing += 1;
            if standing >= STANDING_STILL {
                break;
            }
            continue;
        }
        if marks_seen.contains(&name) {
            // The mark came back to a row it already stood on: the picker
            // wraps, so everything reachable has been read. Without this a
            // wrapping list would spend the whole press budget and outlive
            // the console's refresh deadline.
            break;
        }
        marks_seen.push(name.clone());
        marked = Some(name);
        standing = 0;
    }
    eprintln!("freebuff-driver: catalog: walked the list in {presses} press(es)");

    // The mark goes back to the model the picker was on when it opened, so the
    // catalog names Freebuff's current model and not where the walk stopped.
    let current = default
        .as_ref()
        .and_then(|name| cards.iter().position(|card| &card.name == name));
    for (at, card) in cards.iter_mut().enumerate() {
        card.current = Some(at) == current;
    }
    Ok(picker::Picker {
        cards,
        current,
        section: first.section.clone(),
    })
}

/// Type `/model` and confirm the picker is on screen. One retry, because a
/// startup notice or a survey can swallow the first attempt; after that the
/// screen is not the picker and no more keys are sent.
fn open_picker(pty: &mut PtyProc, screen: &mut Screen) -> Result<picker::Picker, String> {
    for attempt in 0..2 {
        if attempt > 0 {
            // Whatever is on screen is not the list: Escape closes a notice,
            // a dialog, or a picker left half open, and the retry types the
            // command again.
            pty.send(&keys::bytes(Key::Escape));
            wait(pty, screen, STEP_QUIET, STEP_LIMIT)?;
        }
        pty.send(&keys::bytes(Key::Text("/model".to_string())));
        // The composer's slash menu needs a moment to accept Enter as "run
        // this command" rather than "complete this word".
        std::thread::sleep(Duration::from_millis(150));
        pty.send(&keys::bytes(Key::Enter));
        wait(pty, screen, STEP_QUIET, STEP_LIMIT)?;
        if let Some(picker) = capture_is_a_picker(screen) {
            eprintln!(
                "freebuff-driver: catalog: the model picker is open with {} card(s)",
                picker.cards.len()
            );
            return Ok(picker);
        }
    }
    Err("Freebuff did not open its model picker".to_string())
}

/// Pump the terminal until the TUI has answered the last key and gone quiet.
///
/// The quiet clock on its own is not enough: read right after a keystroke it
/// can be satisfied by the pause *before* the repaint. Waiting for fresh
/// output as well is what makes the screen that follows a key the screen a
/// person would see.
fn wait(
    pty: &PtyProc,
    screen: &mut Screen,
    quiet: Duration,
    limit: Duration,
) -> Result<(), String> {
    let started = Instant::now();
    let mut last_output = Instant::now();
    let mut painted = false;
    loop {
        if pty.eof() {
            return Err("Freebuff exited before its model picker appeared".to_string());
        }
        let bytes = pty.drain();
        if !bytes.is_empty() {
            screen.feed(&bytes);
            painted = true;
            last_output = Instant::now();
        }
        match settle_verdicts(last_output.elapsed(), quiet, started.elapsed(), limit) {
            SettleOutcome::Quiet if painted => return Ok(()),
            SettleOutcome::Quiet | SettleOutcome::Waiting => std::thread::sleep(TICK),
            // Read what is there and let the parse decide: a timed-out wait
            // means "look at the screen", never "assume it is ready".
            SettleOutcome::TimedOut => return Ok(()),
        }
    }
}

/// Fold one reading of the picker into what has been read so far.
///
/// The first sighting of a model fixes its place in the list; a later one may
/// fill in a line the earlier one could not see, because a tail can be cut off
/// at the top while the card itself survives.
fn merge(into: &mut Vec<picker::Card>, picker: &picker::Picker) {
    for card in &picker.cards {
        match into.iter_mut().find(|kept| kept.name == card.name) {
            Some(kept) => {
                if kept.tagline.is_none() {
                    kept.tagline = card.tagline.clone();
                }
                if kept.detail.is_none() {
                    kept.detail = card.detail.clone();
                }
                if kept.note.is_none() {
                    kept.note = card.note.clone();
                }
                if kept.section.is_none() {
                    kept.section = card.section.clone();
                }
            }
            None => into.push(card.clone()),
        }
    }
}

fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The same capture the picker parser is pinned on: the picker as a real
    /// Freebuff 0.2.35 window showed it, tail only, copied verbatim with its
    /// leading spaces and bullet characters.
    const CAPTURE: &[&str] = &[
        " Solar Mini 4 · Fast and light · Promotional",
        " May use data for AI training",
        "",
        " MiMo v2.6 Flash Slow · Slow · cheapest · Images",
        " 1x",
        "",
        " PREMIUM · 2–4x cost · lower usage (1–2 hours per day)",
        " DeepSeek V4.1 Flash Slow • high · Slow · DeepSeek · Images",
        " 2x · Recommended · May use data for AI training",
        "",
        "› DeepSeek V4.1 Flash 10/17 • high · Smart & Fast · Images",
        " 3x · May use data for AI training",
    ];

    fn capture() -> Vec<String> {
        CAPTURE.iter().map(|row| row.to_string()).collect()
    }

    #[test]
    fn the_catalog_line_has_the_daemons_descriptor_shape() {
        let picker = picker::parse(&capture()).expect("a picker");
        let catalog = Catalog::from_picker(&picker, 1_762_000_000_000);
        assert_eq!(
            catalog.default.as_deref(),
            Some("DeepSeek V4.1 Flash 10/17 • high")
        );
        assert_eq!(
            catalog.models[3].detail.as_deref(),
            Some("3x · May use data for AI training")
        );
        assert_eq!(catalog.models[3].model, "DeepSeek V4.1 Flash 10/17 • high");
        assert!(catalog.models[3].is_default);
        let line = catalog.to_line();
        let value: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(value["captured_at"], 1_762_000_000_000u64);
        assert_eq!(value["default"], "DeepSeek V4.1 Flash 10/17 • high");
        assert_eq!(value["models"].as_array().unwrap().len(), 4);
        // The field names are the daemon's, so its own descriptor reads this.
        assert_eq!(value["models"][1]["model"], "MiMo v2.6 Flash Slow");
        assert_eq!(value["models"][1]["is_default"], false);
        assert!(value.get("reason").is_none(), "an answer, not a failure");
    }

    #[test]
    fn a_screen_that_is_not_a_picker_captures_nothing() {
        let screen = Screen::new(45, 140);
        assert!(capture_is_a_picker(&screen).is_none());
    }

    #[test]
    fn a_picker_painted_on_the_screen_is_read_through_the_capture_call() {
        let mut screen = Screen::new(45, 140);
        for row in CAPTURE {
            screen.feed(row.as_bytes());
            screen.feed(b"\r\n");
        }
        let picker = capture_is_a_picker(&screen).expect("a picker");
        assert_eq!(picker.cards.len(), 4);
        assert_eq!(picker.current, Some(3));
    }

    #[test]
    fn a_failed_capture_names_a_reason_and_no_models() {
        let line = failure_line("Freebuff did not open its model picker");
        let value: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(value["captured_at"], 0);
        assert_eq!(value["models"].as_array().unwrap().len(), 0);
        assert!(value["default"].is_null());
        assert_eq!(value["reason"], "Freebuff did not open its model picker");
    }
}
