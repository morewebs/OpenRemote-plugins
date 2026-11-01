//! OpenRemote harness driver for the Freebuff CLI.
//!
//! Two modes, both specified in `PROTOCOL.md`:
//!
//! * `--probe` prints one JSON line about the CLI on this machine and exits.
//! * with no arguments it speaks protocol v1 on stdin and stdout: one JSON
//!   command per line in, one JSON event per line out, diagnostics on stderr.
//!
//! The Freebuff TUI needs a real terminal, so the session runs it in a 140x45
//! ConPTY and types prompts into it; the events come from the chat store
//! Freebuff writes, never from scraping the screen.

mod chatdir;
mod logtail;
mod probe;
mod protocol;
mod ptyproc;
mod session;
mod transcript;

use std::io::Write;

const USAGE: &str = "\
freebuff-driver - OpenRemote harness driver for the Freebuff CLI

  freebuff-driver --probe   print one JSON line: available, version, signed_in, reason
  freebuff-driver           speak driver protocol v1 on stdin and stdout
  freebuff-driver --help    this text

Exit codes: 0 ok, 1 the session ended with an error, 2 bad arguments.
";

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let code = match args.first().map(String::as_str) {
        Some("--probe") => {
            // Always exactly one JSON line, always exit 0: an unavailable CLI
            // is an answer, not a driver failure.
            let report = probe::run();
            let mut stdout = std::io::stdout();
            let _ = writeln!(stdout, "{}", report.to_json());
            let _ = stdout.flush();
            0
        }
        Some("--help") | Some("-h") => {
            let _ = std::io::stdout().write_all(USAGE.as_bytes());
            0
        }
        Some(other) => {
            eprintln!("freebuff-driver: unknown argument {other:?}");
            eprintln!("{USAGE}");
            2
        }
        None => match session::run() {
            Ok(()) => 0,
            Err(reason) => {
                eprintln!("freebuff-driver: {reason}");
                1
            }
        },
    };
    std::process::exit(code);
}
