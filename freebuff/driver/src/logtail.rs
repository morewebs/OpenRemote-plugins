//! Following Freebuff's `log.jsonl` for the ends of a turn.
//!
//! The chat store's log is the only place Freebuff says "this turn is over":
//! the transcript itself has no end marker while a turn streams. A log line
//! carries `msg` and, on some lines, the account's id and address, so this
//! module reads the `msg` field only and redacts what it reports.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::PathBuf;

/// Why a turn ended, as far as the log can say.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TurnSignal {
    /// "Main prompt finished": Freebuff's own end-of-turn line.
    Finished,
    /// The turn failed: the line that says so, already redacted and clipped.
    Failed(String),
}

/// Classify one line of the log. Anything that is not a turn boundary is
/// `None`, which is the overwhelming majority of lines.
pub fn classify_line(line: &str) -> Option<TurnSignal> {
    if line.contains("Main prompt finished") {
        return Some(TurnSignal::Finished);
    }
    // `[chat-runtime] Freebuff session over; holding queued messages until
    // rejoin` is deliberately not here. Freebuff writes it in its first second
    // whenever the previous CLI did not exit cleanly - an app closed mid-chat,
    // a killed process - and it says nothing about the turn being run now. It
    // is a startup notice, and treating it as a failure killed the next turn
    // on the spot with a sentence about a session that had already ended.
    if line.contains("Validation failed")
        || line.contains("fetch failed")
        || line.contains("offline")
        || line.contains("Can't reach Freebuff's servers")
        || line.contains("Health check failed")
    {
        return Some(TurnSignal::Failed(redact(&reported_text(line))));
    }
    None
}

/// The part of a log line worth showing a person: its `msg` when the line is
/// JSON, else the line itself.
pub fn reported_text(line: &str) -> String {
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(line) {
        if let Some(msg) = value.get("msg").and_then(|m| m.as_str()) {
            return msg.to_string();
        }
    }
    line.trim().to_string()
}

/// Strip anything credential- or person-shaped and clip to a readable length.
///
/// A Freebuff log line is not guaranteed to be free of the account's address or
/// of a token, and this text is shown in the console, so the safe default wins.
pub fn redact(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for token in text.split_inclusive(char::is_whitespace) {
        let bare = token.trim_end();
        if looks_like_email(bare) || looks_like_secret(bare) {
            out.push_str("<redacted>");
            out.push_str(&token[bare.len()..]);
        } else {
            out.push_str(token);
        }
    }
    clip(&out, 400)
}

fn looks_like_email(token: &str) -> bool {
    let at = match token.find('@') {
        Some(at) => at,
        None => return false,
    };
    let (local, domain) = token.split_at(at);
    let domain = &domain[1..];
    let trimmed = domain.trim_end_matches(|c: char| !c.is_alphanumeric() && c != '.');
    !local.is_empty()
        && !domain.is_empty()
        && trimmed.contains('.')
        && !trimmed.starts_with('.')
        && !trimmed.ends_with('.')
        && local
            .chars()
            .all(|c| c.is_alphanumeric() || "._%+-".contains(c))
}

/// A long opaque run of URL-safe characters: an id or a token, never prose.
fn looks_like_secret(token: &str) -> bool {
    let stripped = token.trim_matches(|c: char| c == '"' || c == ',' || c == '\'' || c == '\\');
    if stripped.len() < 40 {
        return false;
    }
    stripped
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c))
        && stripped.chars().any(|c| c.is_ascii_digit())
        && stripped.chars().any(|c| c.is_ascii_alphabetic())
}

fn clip(text: &str, max: usize) -> String {
    let mut out: String = text.chars().take(max).collect();
    if text.chars().count() > max {
        out.push_str("...");
    }
    out
}

/// A resumable read of one `log.jsonl`.
///
/// The tail starts at the end of the file, so a resumed chat does not replay
/// the end markers of every earlier turn.
#[derive(Debug)]
pub struct LogTail {
    path: PathBuf,
    offset: u64,
    started: bool,
    partial: String,
}

impl LogTail {
    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            offset: 0,
            started: false,
            partial: String::new(),
        }
    }

    /// Read whatever has been appended since the last call.
    pub fn poll(&mut self) -> Vec<TurnSignal> {
        let mut file = match File::open(&self.path) {
            Ok(file) => file,
            Err(_) => return Vec::new(),
        };
        let len = match file.metadata() {
            Ok(meta) => meta.len(),
            Err(_) => return Vec::new(),
        };

        if !self.started {
            // First sight: everything already there belongs to earlier turns.
            self.started = true;
            self.offset = len;
            return Vec::new();
        }
        if len < self.offset {
            // The file was replaced or truncated: read it from the top.
            self.offset = 0;
            self.partial.clear();
        }
        if len == self.offset {
            return Vec::new();
        }
        if file.seek(SeekFrom::Start(self.offset)).is_err() {
            return Vec::new();
        }
        let mut buf = Vec::new();
        if file.read_to_end(&mut buf).is_err() {
            return Vec::new();
        }
        self.offset = len;

        let text = String::from_utf8_lossy(&buf);
        self.partial.push_str(&text);
        let mut signals = Vec::new();
        while let Some(index) = self.partial.find('\n') {
            let line: String = self.partial.drain(..=index).collect();
            if let Some(signal) = classify_line(&line) {
                signals.push(signal);
            }
        }
        // A line longer than the file we have seen so far stays in `partial`
        // until its newline arrives; the log is line oriented.
        signals
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_finished_line_is_the_turn_boundary() {
        let line = r#"{"level":"DEBUG","timestamp":"2026-10-29T12:27:40.149Z","pid":16736,"data":{"outputType":"lastMessage","messageCount":1},"msg":"Main prompt finished"}"#;
        assert_eq!(classify_line(line), Some(TurnSignal::Finished));
    }

    #[test]
    fn validation_and_fetch_failures_are_error_turns() {
        let validation =
            r#"{"level":"ERROR","data":{},"msg":"Validation failed: tool call was rejected"}"#;
        match classify_line(validation) {
            Some(TurnSignal::Failed(text)) => {
                assert!(text.contains("Validation failed"));
                assert!(!text.contains("level"));
            }
            other => panic!("expected a failure, got {other:?}"),
        }

        let fetch = r#"{"level":"ERROR","msg":"fetch failed"}"#;
        assert!(matches!(classify_line(fetch), Some(TurnSignal::Failed(_))));
    }

    #[test]
    fn the_resume_notice_is_not_a_turn_failure() {
        // Written in the first second of a launch after a CLI that did not
        // exit cleanly. It is about a session that already ended, so it must
        // never end the turn being run now.
        let session_over = r#"{"level":"INFO","msg":"[chat-runtime] Freebuff session over; holding queued messages until rejoin"}"#;
        assert_eq!(classify_line(session_over), None);
    }

    #[test]
    fn reachability_errors_are_error_turns() {
        let offline_line = r#"{"level":"ERROR","msg":"gateway reported offline mode"}"#;
        match classify_line(offline_line) {
            Some(TurnSignal::Failed(text)) => {
                assert!(text.contains("offline"));
            }
            other => panic!("expected a failure, got {other:?}"),
        }

        let unreach_line = r#"{"level":"ERROR","msg":"Can't reach Freebuff's servers"}"#;
        match classify_line(unreach_line) {
            Some(TurnSignal::Failed(text)) => {
                assert!(text.contains("Can't reach Freebuff's servers"));
            }
            other => panic!("expected a failure, got {other:?}"),
        }

        let health_line = r#"{"level":"DEBUG","msg":"Health check failed, backing off"}"#;
        match classify_line(health_line) {
            Some(TurnSignal::Failed(text)) => {
                assert!(text.contains("Health check failed, backing off"));
            }
            other => panic!("expected a failure, got {other:?}"),
        }
    }

    #[test]
    fn ordinary_lines_are_not_turn_boundaries() {
        assert_eq!(classify_line(r#"{"msg":"Start agent base3 step 1"}"#), None);
        assert_eq!(classify_line(""), None);
        assert_eq!(classify_line("Main prompt"), None);
    }

    #[test]
    fn an_error_line_never_carries_the_account_address() {
        let line = r#"{"level":"ERROR","userId":"c51338b2","userEmail":"someone@example.com","msg":"Validation failed for someone@example.com"}"#;
        let text = match classify_line(line) {
            Some(TurnSignal::Failed(text)) => text,
            other => panic!("expected a failure, got {other:?}"),
        };
        assert!(!text.contains("someone@example.com"));
        assert!(!text.contains('@'));
        assert!(text.contains("Validation failed"));
    }

    #[test]
    fn a_long_token_is_redacted_but_prose_is_kept() {
        let prose = "the model refused to answer because the prompt was empty";
        assert_eq!(redact(prose), prose);

        let with_token =
            "bearer fbm1_AAEAAUROAmLRGBektoGYvRnj2BSVo9W4r5VmwpcMFZbdCc1I2vJuyBZBHot5Yc7GXNLR5Wpy";
        let redacted = redact(with_token);
        assert!(redacted.contains("<redacted>"));
        assert!(!redacted.contains("AAEAAUROAmLRGB"));
    }

    #[test]
    fn a_very_long_line_is_clipped() {
        let line = "x".repeat(2000);
        assert!(redact(&line).chars().count() <= 403);
    }

    #[test]
    fn the_tail_starts_at_the_end_so_old_turns_do_not_replay() {
        let dir = std::env::temp_dir().join("freebuff-driver-logtail-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("log.jsonl");
        std::fs::write(&path, "{\"msg\":\"Main prompt finished\"}\n").unwrap();

        let mut tail = LogTail::new(path.clone());
        // First poll adopts the current end of file: the old marker is history.
        assert!(tail.poll().is_empty());

        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        writeln!(file, "{{\"msg\":\"a new line\"}}").unwrap();
        writeln!(file, "{{\"msg\":\"Main prompt finished\"}}").unwrap();
        file.flush().unwrap();

        assert_eq!(tail.poll(), vec![TurnSignal::Finished]);
        assert!(tail.poll().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_half_written_line_waits_for_its_newline() {
        let dir = std::env::temp_dir().join("freebuff-driver-logtail-partial");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("log.jsonl");
        std::fs::write(&path, "").unwrap();

        let mut tail = LogTail::new(path.clone());
        assert!(tail.poll().is_empty());

        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        write!(file, "{{\"msg\":\"Main prompt fini").unwrap();
        file.flush().unwrap();
        assert!(tail.poll().is_empty());

        writeln!(file, "shed\"}}").unwrap();
        file.flush().unwrap();
        assert_eq!(tail.poll(), vec![TurnSignal::Finished]);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
