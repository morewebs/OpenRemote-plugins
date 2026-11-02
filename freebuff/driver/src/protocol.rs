//! The JSON-lines protocol, driver side (protocol v1).
//!
//! Input: one JSON object per line on stdin, each a command.
//! Output: one JSON object per line on stdout, each an event.
//! Everything else (diagnostics) goes to stderr.
//!
//! Unknown command types, unknown fields and malformed lines are reported on
//! stderr and otherwise ignored, so a newer daemon does not break an older
//! driver.

use std::io::Write;

use serde::Deserialize;

/// A command from the daemon. Fields this driver does not act on are still
/// parsed, so an older daemon and a newer one take the same path.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Command {
    Spawn {
        cwd: String,
        #[serde(default)]
        model: Option<serde_json::Value>,
        #[serde(default)]
        permission_mode: Option<serde_json::Value>,
        #[serde(default)]
        effort: Option<serde_json::Value>,
        #[serde(default)]
        resume: Option<String>,
        #[serde(default)]
        fast: Option<serde_json::Value>,
    },
    Prompt {
        text: String,
    },
    Interrupt,
    Answer {
        #[serde(default)]
        harness_ref: Option<serde_json::Value>,
        #[serde(default)]
        choice: Option<serde_json::Value>,
    },
    Settings {
        #[serde(default)]
        model: Option<serde_json::Value>,
        #[serde(default)]
        effort: Option<serde_json::Value>,
        #[serde(default)]
        fast: Option<serde_json::Value>,
        #[serde(default)]
        permission_mode: Option<serde_json::Value>,
    },
    Shutdown,
}

impl Command {
    pub fn name(&self) -> &'static str {
        match self {
            Command::Spawn { .. } => "spawn",
            Command::Prompt { .. } => "prompt",
            Command::Interrupt => "interrupt",
            Command::Answer { .. } => "answer",
            Command::Settings { .. } => "settings",
            Command::Shutdown => "shutdown",
        }
    }
}

/// Parse one line of stdin. A blank line is not a command.
pub fn parse_command(line: &str) -> Result<Command, String> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return Err("empty line".to_string());
    }
    serde_json::from_str::<Command>(trimmed).map_err(|e| {
        // Keep the daemon's own payload out of the message: it can be long and
        // it is not this driver's business to echo it.
        format!("malformed command: {e}")
    })
}

/// Write-only handle for the event stream. Every event is one line of JSON on
/// stdout; a write failure means the daemon is gone.
pub struct Events {
    out: Box<dyn Write + Send>,
    dead: bool,
}

impl Events {
    pub fn new() -> Self {
        Self {
            out: Box::new(std::io::stdout()),
            dead: false,
        }
    }

    /// The same emitter over any sink, so tests can read the exact lines.
    #[cfg(test)]
    pub fn over(out: Box<dyn Write + Send>) -> Self {
        Self { out, dead: false }
    }

    /// Emit one event. A closed stdout latches here; the session checks it and
    /// stops instead of writing events into a dead pipe.
    pub fn emit(&mut self, event: serde_json::Value) -> bool {
        if self.dead {
            return false;
        }
        let mut line = event.to_string();
        line.push('\n');
        match self
            .out
            .write_all(line.as_bytes())
            .and_then(|()| self.out.flush())
        {
            Ok(()) => true,
            Err(_) => {
                self.dead = true;
                false
            }
        }
    }

    /// True once the event stream can no longer be written.
    pub fn dead(&self) -> bool {
        self.dead
    }

    pub fn initialized(
        &mut self,
        session_ref: &str,
        model: Option<&serde_json::Value>,
        permission_mode: Option<&serde_json::Value>,
    ) -> bool {
        self.emit(serde_json::json!({
            "type": "initialized",
            "session_ref": session_ref,
            "model": model,
            "permission_mode": permission_mode,
        }))
    }

    pub fn text(&mut self, text: &str) -> bool {
        self.emit(serde_json::json!({ "type": "text", "text": text }))
    }

    /// The model a running chat now runs.
    ///
    /// A spawn reports the model it started on through `initialized`; this is
    /// the event for a model change inside a running chat, which the daemon
    /// folds into the session so the console's chip follows what the driver
    /// verified rather than what was asked for.
    #[allow(dead_code)] // The live settings path emits this next.
    pub fn model_reported(&mut self, model: &str) -> bool {
        self.emit(serde_json::json!({ "type": "model_reported", "model": model }))
    }

    pub fn text_delta(&mut self, text: &str) -> bool {
        self.emit(serde_json::json!({ "type": "text_delta", "text": text }))
    }

    pub fn reasoning(&mut self, text: &str) -> bool {
        self.emit(serde_json::json!({ "type": "reasoning", "text": text }))
    }

    pub fn reasoning_delta(&mut self, text: &str) -> bool {
        self.emit(serde_json::json!({ "type": "reasoning_delta", "text": text }))
    }

    pub fn tool_started(
        &mut self,
        tool_use_id: &str,
        name: &str,
        input: &serde_json::Value,
    ) -> bool {
        self.emit(serde_json::json!({
            "type": "tool_started",
            "tool_use_id": tool_use_id,
            "name": name,
            "input": input,
        }))
    }

    pub fn tool_result(
        &mut self,
        tool_use_id: &str,
        name: &str,
        text: &str,
        is_error: bool,
    ) -> bool {
        self.emit(serde_json::json!({
            "type": "tool_result",
            "tool_use_id": tool_use_id,
            "name": name,
            "text": text,
            "is_error": is_error,
        }))
    }

    pub fn turn_completed(
        &mut self,
        subtype: &str,
        coarse: &str,
        is_error: bool,
        error_message: Option<&str>,
    ) -> bool {
        self.emit(serde_json::json!({
            "type": "turn_completed",
            "subtype": subtype,
            "coarse": coarse,
            "is_error": is_error,
            "error_message": error_message,
        }))
    }

    pub fn stderr_line(&mut self, line: &str) -> bool {
        self.emit(serde_json::json!({ "type": "stderr", "line": line }))
    }

    pub fn closed(&mut self) -> bool {
        self.emit(serde_json::json!({ "type": "closed" }))
    }
}

impl Default for Events {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_every_command_of_the_v1_table() {
        assert_eq!(
            parse_command(r#"{"type":"spawn","cwd":"C:\\work"}"#).unwrap(),
            Command::Spawn {
                cwd: "C:\\work".into(),
                model: None,
                permission_mode: None,
                effort: None,
                resume: None,
                fast: None,
            }
        );
        assert_eq!(
            parse_command(r#"{"type":"prompt","text":"hi"}"#).unwrap(),
            Command::Prompt { text: "hi".into() }
        );
        assert_eq!(
            parse_command(r#"{"type":"interrupt"}"#).unwrap(),
            Command::Interrupt
        );
        assert_eq!(
            parse_command(r#"{"type":"shutdown"}"#).unwrap(),
            Command::Shutdown
        );
        assert!(matches!(
            parse_command(r#"{"type":"answer","harness_ref":"a","choice":"b"}"#).unwrap(),
            Command::Answer { .. }
        ));
        assert!(matches!(
            parse_command(r#"{"type":"settings","model":"x"}"#).unwrap(),
            Command::Settings { .. }
        ));
    }

    #[test]
    fn spawn_keeps_resume_and_ignores_unknown_fields() {
        let command = parse_command(
            r#"{"type":"spawn","cwd":"/tmp/x","resume":"2026-01-01T00-00-00.000Z","something_new":true}"#,
        )
        .unwrap();
        match command {
            Command::Spawn { cwd, resume, .. } => {
                assert_eq!(cwd, "/tmp/x");
                assert_eq!(resume.as_deref(), Some("2026-01-01T00-00-00.000Z"));
            }
            other => panic!("expected spawn, got {other:?}"),
        }
    }

    #[test]
    fn malformed_and_unknown_commands_are_errors_not_panics() {
        assert!(parse_command("not json").is_err());
        assert!(parse_command(r#"{"type":"teleport"}"#).is_err());
        assert!(parse_command("   ").is_err());
        // A spawn without a cwd cannot start a session.
        assert!(parse_command(r#"{"type":"spawn"}"#).is_err());
    }

    #[test]
    fn turn_completed_always_carries_all_four_fields() {
        let (mut events, buffer) = capture();
        assert!(events.turn_completed("finished", "completed", false, None));
        assert_eq!(
            lines(&buffer),
            vec![
                r#"{"coarse":"completed","error_message":null,"is_error":false,"subtype":"finished","type":"turn_completed"}"#
            ]
        );
    }

    #[test]
    fn an_error_turn_carries_its_message() {
        let (mut events, buffer) = capture();
        assert!(events.turn_completed("error", "failed", true, Some("fetch failed")));
        let line = lines(&buffer).remove(0);
        let value: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(value["is_error"], serde_json::json!(true));
        assert_eq!(value["error_message"], serde_json::json!("fetch failed"));
    }

    #[test]
    fn an_interrupted_turn_says_interrupted_and_is_not_an_error() {
        let (mut events, buffer) = capture();
        assert!(events.turn_completed("interrupted", "interrupted", false, None));
        let value: serde_json::Value = serde_json::from_str(&lines(&buffer)[0]).unwrap();
        assert_eq!(value["subtype"], serde_json::json!("interrupted"));
        assert_eq!(value["is_error"], serde_json::json!(false));
    }

    #[test]
    fn the_event_stream_is_exactly_one_json_object_per_line() {
        let (mut events, buffer) = capture();
        assert!(events.initialized("2026-10-31T20-33-15.026Z", None, None));
        assert!(events.text("hello"));
        assert!(events.tool_started("call-1", "read_files", &serde_json::json!({"path": "a"})));
        assert!(events.tool_result("call-1", "read_files", "body", false));
        assert!(events.closed());

        for line in lines(&buffer) {
            let value: serde_json::Value = serde_json::from_str(&line).expect("a JSON line");
            assert!(value.get("type").is_some(), "every event names its type");
        }
    }

    #[test]
    fn initialized_reports_a_null_model_because_freebuff_has_none_to_report() {
        let (mut events, buffer) = capture();
        assert!(events.initialized("chat-1", None, Some(&serde_json::json!("plan"))));
        let value: serde_json::Value = serde_json::from_str(&lines(&buffer)[0]).unwrap();
        assert_eq!(value["session_ref"], serde_json::json!("chat-1"));
        assert_eq!(value["model"], serde_json::Value::Null);
        assert_eq!(value["permission_mode"], serde_json::json!("plan"));
    }

    #[test]
    fn a_closed_sink_latches_dead_instead_of_panicking() {
        struct Broken;
        impl Write for Broken {
            fn write(&mut self, _buf: &[u8]) -> std::io::Result<usize> {
                Err(std::io::Error::new(std::io::ErrorKind::BrokenPipe, "gone"))
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        let mut events = Events::over(Box::new(Broken));
        assert!(!events.emit(serde_json::json!({"type": "closed"})));
        assert!(events.dead());
        // Further events are refused rather than retried.
        assert!(!events.emit(serde_json::json!({"type": "closed"})));
    }

    fn capture() -> (Events, std::sync::Arc<std::sync::Mutex<Vec<u8>>>) {
        let buffer = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        (
            Events::over(Box::new(Sink(std::sync::Arc::clone(&buffer)))),
            buffer,
        )
    }

    fn lines(buffer: &std::sync::Arc<std::sync::Mutex<Vec<u8>>>) -> Vec<String> {
        let bytes = buffer.lock().unwrap().clone();
        String::from_utf8(bytes)
            .unwrap()
            .lines()
            .map(str::to_string)
            .collect()
    }

    struct Sink(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

    impl Write for Sink {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
}
