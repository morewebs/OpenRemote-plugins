# OpenRemote Freebuff harness plugin

Freebuff CLI as a harness inside [OpenRemote](../OpenRemote). It ships as an
OpenRemote plugin: the plugin folder declares a harness, and this folder's own
driver process translates between OpenRemote's driver protocol and Freebuff.

Nothing Freebuff-specific lives in OpenRemote's source. Remove this folder and
the harness is gone.

```
openremote.json                     the plugin manifest, with the harness block
bin/windows-x64/freebuff-driver.exe the driver, built from driver/
driver/                             Rust crate: freebuff-driver
PROTOCOL.md                         the protocol this driver speaks
README.md
```

## Requirements

* Windows 10 or newer, x64. The only shipped binary is Windows; the manifest
  names `bin/freebuff-driver` as the fallback for other platforms, and that
  build does not exist yet.
* Freebuff CLI **0.2.35 or newer**, installed at
  `%USERPROFILE%\.config\manicode\freebuff.exe`.
* A Freebuff sign-in. Run `freebuff login` once in a real terminal. This plugin
  never handles your credentials: it checks that `credentials.json` exists and
  never reads it.
* About 2 seconds per chat start: the driver asks the CLI for its version, then
  waits for the TUI to paint before it types anything.

The npm `freebuff` shim is deliberately unused. Its launcher runs an update
check that waits on a TTY, so a console-less child hangs on it; the driver
resolves the real executable instead.

## Install

Install it as an OpenRemote plugin, from this folder or from its git URL, then
enable it. The harness appears in New chat as **Freebuff**. Nothing needs a
daemon restart: OpenRemote re-probes the harness list when a plugin declaring a
harness is installed, enabled, disabled, removed or updated.

Checking the probe by hand:

```sh
bin/windows-x64/freebuff-driver.exe --probe
{"available":true,"reason":null,"signed_in":true,"version":"0.2.35"}
```

That one line is what the harness row is built from. `available: false` comes
with a plain `reason` such as `Freebuff CLI not found at ...`, and the row says
so instead of pretending to be ready.

## What works

* The harness row, with the CLI's real version and its signed-in state.
* A new chat: the driver starts Freebuff in a 140x45 pseudo terminal in the
  chat's working directory and types your prompt into it.
* Assistant text, the model's reasoning, and tool calls with their results, all
  read from Freebuff's own chat transcript rather than scraped off the screen.
* A finished turn, when Freebuff ends one. A turn can never hang for ever: if
  the transcript and the log stop moving for 240 seconds, the turn is closed as
  an error that says so, and you can retry instead of watching a spinner.
* Stop and resume: the chat id Freebuff writes is reported as `session_ref`,
  and resuming starts the CLI with `--continue <id>`.
* Interrupt: Escape, then Ctrl-C if the TUI is still working, then a turn that
  is honestly reported as interrupted.
* Shutdown: the CLI is terminated with the chat.

## What is honestly unsupported

These are declared as gaps rather than faked, because the CLI gives no way to
know them:

* **No model picker.** Freebuff decides its own model. The manifest declares no
  models, and `initialized` reports `model: null`.
* **No effort and no fast mode.** A running chat cannot change either. A
  settings change is refused with a message that says to stop and resume.
* **No approvals.** Freebuff LITE runs its tools without asking. The driver
  declares no approval flow and never answers one; if the TUI ever stops on a
  question, you get a diagnostic quoting the screen rather than a guessed
  decision.
* **No context window and no cost.** The CLI reports neither to a client.
* **No sign-in relay.** If you are signed out, the chat refuses to start and
  tells you to run `freebuff login` in a terminal.
* **No prompt argument, no `--print`, no `--json`.** Freebuff 0.2.35 has none
  of these, which is why the driver owns a pseudo terminal at all.

Two more limits worth knowing:

* The chat store under `%USERPROFILE%\.config\manicode\projects\` is internal to
  Freebuff and can move in any release. If a Freebuff upgrade changes it, this
  plugin needs an update. That is also why the driver refuses a CLI older than
  0.2.35 instead of half working.
* One prompt at a time. A second prompt while a turn is open is refused, and
  the driver never swallows one silently.

## Build

```sh
cd driver
cargo build --release
# the binary is at target/release/freebuff-driver.exe
cp target/release/freebuff-driver.exe ../bin/windows-x64/freebuff-driver.exe
```

Gates, all green for the committed tree:

```sh
cargo test                      # 48 tests, the parsers and the event shapes
cargo clippy --all-targets -- -D warnings
cargo fmt --all --check
```

If your system drive is short of space, point the build somewhere else:
`CARGO_TARGET_DIR=D:\cargo\freebuff-driver cargo build --release`.

## What has been verified, and what has not

Verified on the machine this was built on, against Freebuff 0.2.35:

* `--probe` reporting `{"available":true,"reason":null,"signed_in":true,"version":"0.2.35"}`.
* A pseudo terminal running the Freebuff TUI: it paints, stays alive, accepts a
  bracketed paste, and answers `-v` with `0.2.35` and exit code 0.
* A full session with no prompt: spawn, chat id discovery, `initialized`, and a
  clean `shutdown` to `closed`.
* A submitted prompt reaching Freebuff: typing `/help` made the CLI act on it.

Not verified, because the CLI could not reach its own servers from this machine
during the build (`Health check failed, backing off` and
`[freebuff-catalog] fetch failed` in Freebuff's own log, with the same result on
two attempts while this machine's network was otherwise fine): a real turn's
text and tool events, a `turn_completed` of subtype `finished`, and a resume.
The driver maps those server failures to an error turn with the CLI's own
message, which is what you see instead of an answer. Everything past
`initialized` needs a run on a machine where Freebuff's backend is reachable.

## License

Not set yet: the repository ships no license file, and that is a decision for
its owner. OpenRemote itself is AGPL-3.0. Harness names and marks belong to
their owners; this plugin is not affiliated with Freebuff.
