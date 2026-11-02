//! The pseudo terminal the Freebuff TUI runs in.
//!
//! Two Windows details are load-bearing, both measured on this machine:
//!
//! * The writable half of a `portable_pty` master must stay alive for the whole
//!   session. The ConPTY handle lives inside an `Arc<Mutex<Inner>>` shared by
//!   the master and the slave, so dropping both closes the pseudo console and
//!   kills the child instantly with exit code 0xC0000142.
//! * `CreatePseudoConsole` is called with `INHERIT_CURSOR`, so conhost asks the
//!   client for a cursor position (`ESC[6n`) and blocks the child's console
//!   init until it gets an answer. Without the reply, `cmd.exe`, `node` and
//!   `freebuff` all hang with no output at all.
//!
//! The read half is drained by a thread into a channel, because a full pipe
//! would otherwise block the child; the write half is drained by a second
//! thread, so a prompt and a cursor reply never interleave mid-escape.

use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};

use portable_pty::{native_pty_system, CommandBuilder, MasterPty, PtySize};

/// The TUI lays itself out for this size, as the design specifies.
pub const ROWS: u16 = 45;
pub const COLS: u16 = 140;

pub struct PtyProc {
    child: Box<dyn portable_pty::Child + Send + Sync>,
    cmd_tx: mpsc::Sender<Vec<u8>>,
    data_rx: mpsc::Receiver<Vec<u8>>,
    eof: Arc<AtomicBool>,
    reader_error: Arc<Mutex<Option<String>>>,
    // Keeps the ConPTY handle open. See the module comment.
    _master: Box<dyn MasterPty + Send>,
}

impl PtyProc {
    /// Start `program` with `args` in a 140x45 pseudo terminal.
    pub fn spawn(
        program: &std::path::Path,
        args: &[String],
        cwd: &std::path::Path,
    ) -> Result<Self, String> {
        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(PtySize {
                rows: ROWS,
                cols: COLS,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| format!("could not open a pseudo terminal: {e}"))?;

        let mut builder = CommandBuilder::new(program);
        for arg in args {
            builder.arg(arg);
        }
        configure_command_env(&mut builder);
        builder.cwd(cwd);

        let child = pair
            .slave
            .spawn_command(builder)
            .map_err(|e| format!("could not start {}: {e}", program.display()))?;
        drop(pair.slave);

        let mut reader = pair
            .master
            .try_clone_reader()
            .map_err(|e| format!("could not read the pseudo terminal: {e}"))?;
        let mut writer = pair
            .master
            .take_writer()
            .map_err(|e| format!("could not write to the pseudo terminal: {e}"))?;

        let (cmd_tx, cmd_rx) = mpsc::channel::<Vec<u8>>();
        let (data_tx, data_rx) = mpsc::channel::<Vec<u8>>();
        let eof = Arc::new(AtomicBool::new(false));
        let reader_error = Arc::new(Mutex::new(None));

        // Writes: prompt text and cursor replies, one buffer at a time.
        std::thread::spawn(move || {
            while let Ok(bytes) = cmd_rx.recv() {
                if writer.write_all(&bytes).is_err() {
                    break;
                }
                let _ = writer.flush();
            }
        });

        // Reads: hand the bytes on and answer the one terminal query the
        // pseudo console waits for.
        let reply_tx = cmd_tx.clone();
        let eof_flag = Arc::clone(&eof);
        let err_slot = Arc::clone(&reader_error);
        std::thread::spawn(move || {
            let mut buf = [0u8; 8192];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        if contains(&buf[..n], b"\x1b[6n") {
                            let _ = reply_tx.send(CURSOR_REPORT.to_vec());
                        }
                        if data_tx.send(buf[..n].to_vec()).is_err() {
                            break;
                        }
                    }
                    Err(e) => {
                        if let Ok(mut slot) = err_slot.lock() {
                            *slot = Some(format!("{e}"));
                        }
                        break;
                    }
                }
            }
            eof_flag.store(true, Ordering::SeqCst);
        });

        Ok(Self {
            child,
            cmd_tx,
            data_rx,
            eof,
            reader_error,
            _master: pair.master,
        })
    }

    /// Queue bytes for the child. The writer thread sends them in order.
    pub fn send(&self, bytes: &[u8]) {
        let _ = self.cmd_tx.send(bytes.to_vec());
    }

    /// Everything the child has printed since the last call, without waiting.
    pub fn drain(&self) -> Vec<u8> {
        let mut out = Vec::new();
        while let Ok(chunk) = self.data_rx.try_recv() {
            out.extend_from_slice(&chunk);
        }
        out
    }

    /// True once the read half reported end of file: the CLI is gone.
    pub fn eof(&self) -> bool {
        self.eof.load(Ordering::SeqCst)
    }

    pub fn reader_error(&self) -> Option<String> {
        self.reader_error.lock().ok().and_then(|slot| slot.clone())
    }

    pub fn pid(&self) -> Option<u32> {
        self.child.process_id()
    }

    pub fn try_wait(&mut self) -> Option<u32> {
        match self.child.try_wait() {
            Ok(Some(status)) => Some(status.exit_code()),
            _ => None,
        }
    }

    pub fn kill(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Check if a local address is listening and accepting TCP connections within the timeout.
pub fn is_port_listening(addr: &str, timeout: std::time::Duration) -> bool {
    use std::net::{SocketAddr, TcpStream};
    if let Ok(socket_addr) = addr.parse::<SocketAddr>() {
        TcpStream::connect_timeout(&socket_addr, timeout).is_ok()
    } else {
        false
    }
}

fn var_is_set(key: &str) -> bool {
    std::env::var(key)
        .map(|v| !v.trim().is_empty())
        .unwrap_or(false)
}

/// Inherit environment variables from the parent process into `CommandBuilder`,
/// ensuring proxy variables are propagated and auto-detecting a local proxy if needed.
pub fn configure_command_env(builder: &mut CommandBuilder) {
    // 1. Inherit all environment variables from parent process.
    for (key, val) in std::env::vars() {
        builder.env(key, val);
    }

    // 2. Explicitly ensure standard proxy variables (uppercase and lowercase) are passed.
    const PROXY_VARS: &[&str] = &[
        "HTTP_PROXY",
        "HTTPS_PROXY",
        "ALL_PROXY",
        "NO_PROXY",
        "http_proxy",
        "https_proxy",
        "all_proxy",
        "no_proxy",
    ];
    for var in PROXY_VARS {
        if let Ok(val) = std::env::var(var) {
            builder.env(var, val);
        }
    }

    // 3. If HTTP_PROXY or HTTPS_PROXY is not set, check if 127.0.0.1:10808 is listening or available,
    // and set HTTP_PROXY and HTTPS_PROXY to http://127.0.0.1:10808.
    let has_http = var_is_set("HTTP_PROXY") || var_is_set("http_proxy");
    let has_https = var_is_set("HTTPS_PROXY") || var_is_set("https_proxy");

    if (!has_http || !has_https)
        && is_port_listening("127.0.0.1:10808", std::time::Duration::from_millis(150))
    {
        if !has_http {
            builder.env("HTTP_PROXY", "http://127.0.0.1:10808");
            builder.env("http_proxy", "http://127.0.0.1:10808");
        }
        if !has_https {
            builder.env("HTTPS_PROXY", "http://127.0.0.1:10808");
            builder.env("https_proxy", "http://127.0.0.1:10808");
        }
    }

    builder.env("TERM", "xterm-256color");
}

/// What a terminal answers `ESC[6n` with: cursor at row 1, column 1.
pub const CURSOR_REPORT: &[u8] = b"\x1b[1;1R";

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

/// Reduce a terminal byte stream to the text a person would read.
///
/// This is for diagnostics only: the transcript is the event source, and the
/// screen is quoted when Freebuff shows a prompt the driver cannot answer.
pub fn strip_ansi(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            match chars.peek().copied() {
                Some('[') => {
                    // CSI: parameter bytes, then one final byte in @..~
                    chars.next();
                    while let Some(&next) = chars.peek() {
                        chars.next();
                        if ('@'..='~').contains(&next) {
                            break;
                        }
                    }
                }
                Some(']') => {
                    // OSC: ends at BEL or at ESC \
                    chars.next();
                    while let Some(next) = chars.next() {
                        if next == '\u{7}' {
                            break;
                        }
                        if next == '\u{1b}' {
                            if chars.peek() == Some(&'\\') {
                                chars.next();
                            }
                            break;
                        }
                    }
                }
                Some('(') | Some(')') | Some('*') | Some('+') => {
                    chars.next();
                    chars.next();
                }
                Some(_) => {
                    chars.next();
                }
                None => {}
            }
            continue;
        }
        match c {
            '\n' | '\t' => out.push(c),
            c if c.is_control() => {}
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_cursor_query_is_answered_with_a_vt100_report() {
        assert!(contains(b"abc\x1b[6ndef", b"\x1b[6n"));
        assert!(!contains(b"abc[6ndef", b"\x1b[6n"));
        assert_eq!(CURSOR_REPORT, b"\x1b[1;1R");
    }

    #[test]
    fn strip_ansi_keeps_the_readable_text_of_a_tui_frame() {
        let frame =
            "\x1b[?25l\x1b[HFreebuff\x1b[38;2;1;2;3m ready\x1b[m\r\n> hello\x1b]0;title\u{7}";
        let text = strip_ansi(frame.as_bytes());
        assert_eq!(text, "Freebuff ready\n> hello");
    }

    #[test]
    fn strip_ansi_handles_an_osc_terminated_by_escape_backslash() {
        let frame = "\x1b]8;;https://example.com\x1b\\link\x1b]8;;\x1b\\";
        assert_eq!(strip_ansi(frame.as_bytes()), "link");
    }

    #[test]
    fn strip_ansi_drops_control_bytes_but_keeps_unicode_text() {
        let frame = "\x07\x1b[31mhéllo \u{2502} wörld\x1b[0m\x00";
        assert_eq!(strip_ansi(frame.as_bytes()), "héllo \u{2502} wörld");
    }

    #[test]
    fn strip_ansi_survives_a_truncated_escape_sequence() {
        assert_eq!(strip_ansi(b"text\x1b["), "text");
        assert_eq!(strip_ansi(b"text\x1b"), "text");
    }

    #[test]
    fn port_listening_detects_open_socket() {
        use std::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
        let port = listener.local_addr().unwrap().port();
        let addr = format!("127.0.0.1:{port}");
        assert!(is_port_listening(
            &addr,
            std::time::Duration::from_millis(200)
        ));
        drop(listener);
    }
}
