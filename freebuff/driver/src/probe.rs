//! Locating the Freebuff CLI, reading its version, and answering `--probe`.
//!
//! The npm shim (`freebuff.cmd`) is deliberately never used: its launcher runs
//! an update check that waits on a TTY, so a console-less child hangs on it.
//! Only the real executable next to Freebuff's own config is considered, then a
//! `freebuff` binary found on PATH.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// The oldest Freebuff CLI this driver has been measured against. The chat
/// store layout below is internal to Freebuff and can move between releases,
/// so an older CLI is refused with a clear message instead of half working.
pub const MIN_VERSION: (u32, u32, u32) = (0, 2, 35);

/// One `--probe` answer. Every field is always present: `null` where the
/// driver does not know, never a guess.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProbeReport {
    pub available: bool,
    pub version: Option<String>,
    pub signed_in: Option<bool>,
    pub reason: Option<String>,
}

impl ProbeReport {
    pub fn unavailable(reason: impl Into<String>) -> Self {
        Self {
            available: false,
            version: None,
            signed_in: None,
            reason: Some(reason.into()),
        }
    }

    /// The single JSON line the daemon reads.
    pub fn to_json(&self) -> String {
        serde_json::json!({
            "available": self.available,
            "version": self.version,
            "signed_in": self.signed_in,
            "reason": self.reason,
        })
        .to_string()
    }
}

/// Freebuff's own config directory: `%USERPROFILE%\.config\manicode` on
/// Windows, `$HOME/.config/manicode` elsewhere.
pub fn manicode_dir() -> Option<PathBuf> {
    let home = home_dir()?;
    Some(home.join(".config").join("manicode"))
}

fn home_dir() -> Option<PathBuf> {
    let var = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
    match std::env::var_os(var) {
        Some(v) if !v.is_empty() => Some(PathBuf::from(v)),
        _ => match std::env::var_os("HOME") {
            Some(v) if !v.is_empty() => Some(PathBuf::from(v)),
            _ => None,
        },
    }
}

/// Resolve the Freebuff executable.
///
/// Order, as in the design: the binary beside Freebuff's own config, then
/// `freebuff` on PATH. On Windows the PATH search skips `.cmd` and `.bat`
/// shims, because the npm launcher waits on a TTY and would hang the driver.
pub fn freebuff_path() -> Option<PathBuf> {
    if let Some(dir) = manicode_dir() {
        let candidates: &[&str] = if cfg!(windows) {
            &["freebuff.exe", "freebuff"]
        } else {
            &["freebuff", "freebuff.exe"]
        };
        for name in candidates {
            let candidate = dir.join(name);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }

    let exe_names: &[&str] = if cfg!(windows) {
        &["freebuff.exe"]
    } else {
        &["freebuff"]
    };
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        for name in exe_names {
            let candidate = dir.join(name);
            if candidate.is_file() {
                // A `.cmd`/`.bat` next to it is irrelevant: we look for the
                // real executable name only, never an interpreter shim.
                return Some(candidate);
            }
        }
    }
    None
}

/// Whether Freebuff looks signed in: only the presence of its credentials
/// file is checked. Its contents are never read, printed or logged.
pub fn credentials_present() -> Option<bool> {
    let dir = manicode_dir()?;
    Some(dir.join("credentials.json").is_file())
}

struct Captured {
    stdout: String,
    stderr: String,
    status_ok: bool,
}

/// Run a program to completion with a timeout, capturing its output.
///
/// Used only for `freebuff -v`: that path prints and exits even without a TTY,
/// and it is the cheap way to learn a version. A run that outlives the timeout
/// is killed and reported as an error.
fn run_capture(program: &Path, args: &[&str], timeout: Duration) -> Result<Captured, String> {
    let mut cmd = Command::new(program);
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }

    let mut child = cmd
        .spawn()
        .map_err(|e| format!("could not run {}: {e}", program.display()))?;

    let (tx, rx) = mpsc::channel::<(String, String)>();
    let mut out = child.stdout.take();
    let mut err = child.stderr.take();
    std::thread::spawn(move || {
        use std::io::Read;
        let mut stdout = String::new();
        let mut stderr = String::new();
        if let Some(o) = out.as_mut() {
            let _ = o.read_to_string(&mut stdout);
        }
        if let Some(e) = err.as_mut() {
            let _ = e.read_to_string(&mut stderr);
        }
        let _ = tx.send((stdout, stderr));
    });

    let started = Instant::now();
    let status_ok = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status.success(),
            Ok(None) => {
                if started.elapsed() > timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(format!(
                        "{} {} did not exit within {} seconds",
                        program.display(),
                        args.join(" "),
                        timeout.as_secs()
                    ));
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(e) => return Err(format!("could not wait for {}: {e}", program.display())),
        }
    };

    let (stdout, stderr) = rx
        .recv_timeout(Duration::from_millis(500))
        .map_err(|_| "could not read the version output".to_string())?;
    Ok(Captured {
        stdout,
        stderr,
        status_ok,
    })
}

/// Run `freebuff -v` and pick the first `x.y.z` in its output.
pub fn read_version(exe: &Path) -> Result<String, String> {
    let captured = run_capture(exe, &["-v"], Duration::from_secs(4))?;
    let mut combined = captured.stdout.clone();
    combined.push('\n');
    combined.push_str(&captured.stderr);

    if let Some(version) = find_version(&combined) {
        return Ok(version);
    }
    let first_line = combined
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("");
    Err(format!(
        "`{} -v` printed no version (exit ok: {}, first line: {})",
        exe.display(),
        captured.status_ok,
        clip(first_line, 120)
    ))
}

/// The first dotted numeric version in a blob of text.
pub fn find_version(text: &str) -> Option<String> {
    let bytes: Vec<char> = text.chars().collect();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i].is_ascii_digit() {
            let start = i;
            let mut parts = 0;
            let mut end = i;
            while end < bytes.len() {
                let c = bytes[end];
                if c.is_ascii_digit() {
                    end += 1;
                } else if c == '.' && parts < 2 {
                    parts += 1;
                    end += 1;
                } else {
                    break;
                }
            }
            if parts == 2 {
                let candidate: String = bytes[start..end].iter().collect();
                if candidate.split('.').all(|p| !p.is_empty() && p.len() <= 4) {
                    return Some(candidate);
                }
            }
            i = end.max(i + 1);
        } else {
            i += 1;
        }
    }
    None
}

/// Compare dotted versions numerically.
pub fn version_at_least(version: &str, minimum: (u32, u32, u32)) -> bool {
    let mut it = version.split('.').map(|p| {
        p.chars()
            .take_while(char::is_ascii_digit)
            .collect::<String>()
            .parse::<u32>()
            .unwrap_or(0)
    });
    let got = (
        it.next().unwrap_or(0),
        it.next().unwrap_or(0),
        it.next().unwrap_or(0),
    );
    got >= minimum
}

fn clip(text: &str, max: usize) -> String {
    let mut out: String = text.chars().take(max).collect();
    if text.chars().count() > max {
        out.push_str("...");
    }
    out
}

/// Build the probe answer. Never panics, never guesses: a missing CLI, a
/// failing `-v` or an unreadable config directory each produce a plain reason
/// the console can show.
pub fn run() -> ProbeReport {
    let exe = match freebuff_path() {
        Some(exe) => exe,
        None => {
            let where_ = manicode_dir()
                .map(|d| {
                    d.join(if cfg!(windows) {
                        "freebuff.exe"
                    } else {
                        "freebuff"
                    })
                })
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "~/.config/manicode/freebuff".to_string());
            return ProbeReport::unavailable(format!("Freebuff CLI not found at {where_}"));
        }
    };

    let version = match read_version(&exe) {
        Ok(version) => version,
        Err(reason) => return ProbeReport::unavailable(reason),
    };

    let signed_in = credentials_present();

    if !version_at_least(&version, MIN_VERSION) {
        let wanted = format!("{}.{}.{}", MIN_VERSION.0, MIN_VERSION.1, MIN_VERSION.2);
        return ProbeReport {
            available: false,
            version: Some(version.clone()),
            signed_in,
            reason: Some(format!(
                "Freebuff CLI {version} is older than the {wanted} this driver needs"
            )),
        };
    }

    ProbeReport {
        available: true,
        version: Some(version),
        signed_in,
        reason: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probe_json_available_has_every_key_and_null_reason() {
        let report = ProbeReport {
            available: true,
            version: Some("0.2.35".into()),
            signed_in: Some(true),
            reason: None,
        };
        assert_eq!(
            report.to_json(),
            r#"{"available":true,"reason":null,"signed_in":true,"version":"0.2.35"}"#
        );
    }

    #[test]
    fn probe_json_unavailable_reports_a_reason_and_no_version() {
        let report = ProbeReport::unavailable("Freebuff CLI not found at x");
        let value: serde_json::Value = serde_json::from_str(&report.to_json()).unwrap();
        assert_eq!(value["available"], serde_json::json!(false));
        assert_eq!(value["version"], serde_json::Value::Null);
        assert_eq!(value["signed_in"], serde_json::Value::Null);
        assert_eq!(
            value["reason"],
            serde_json::json!("Freebuff CLI not found at x")
        );
    }

    #[test]
    fn version_is_the_first_dotted_triple() {
        assert_eq!(find_version("0.2.35\n"), Some("0.2.35".into()));
        assert_eq!(
            find_version("\u{1b}[?25hFreebuff 0.2.35 (build 7)"),
            Some("0.2.35".into())
        );
        assert_eq!(find_version("no version here"), None);
        assert_eq!(find_version("1.2"), None);
        assert_eq!(find_version("10.20.30"), Some("10.20.30".into()));
    }

    #[test]
    fn version_compare_is_numeric_not_lexicographic() {
        assert!(version_at_least("0.2.35", MIN_VERSION));
        assert!(!version_at_least("0.2.9", MIN_VERSION));
        assert!(version_at_least("0.10.0", MIN_VERSION));
        assert!(!version_at_least("0.2.34", MIN_VERSION));
    }

    #[test]
    fn credentials_presence_is_a_bool_never_a_value() {
        // Whatever this machine says, the answer carries no credential text.
        if let Some(present) = credentials_present() {
            let line = format!("{present}");
            assert!(line == "true" || line == "false");
        }
    }
}
