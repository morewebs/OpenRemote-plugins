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
models.json                         the catalog this machine captured (run time)
PROTOCOL.md                         the protocol this driver speaks
README.md
```

`models.json` is not source and is not committed: the driver writes it when it
reads Freebuff's picker, and the manifest ships the list a fresh install starts
from.

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
  waits for the TUI to paint before it types anything. A chat that has to move
  Freebuff to a different model takes a few seconds more, because the driver
  reads the picker after every key.

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
* Freebuff's real model list, read from Freebuff's own picker: every model the
  CLI offers, with its cost tier (`3x · May use data for AI training`) on the
  row and its data-use or plan notice on hover. The manifest ships the list
  captured on 0.2.35, so a fresh install has it before the first chat.
* A new chat in the model you picked: the driver opens `/model`, reads the row
  the picker marks, walks to your model with the arrow keys, selects it, then
  reopens the list to prove the mark landed there. The chat's chip shows the
  model the driver verified, which is what Freebuff is actually running.
* A model change in a running chat, between turns. Same walk, same proof, and
  the chip follows the model the picker showed rather than the one that was
  asked for.
* A new chat: the driver starts Freebuff in a 140x45 pseudo terminal in the
  chat's working directory and types your prompt into it.
* Assistant text, the model's reasoning, and tool calls with their results, all
  read from Freebuff's own chat transcript rather than scraped off the screen.
* Freebuff's own send confirmation: when it asks "Enter: confirm and send" for a
  message you already sent, the driver presses that Enter (at most three times a
  turn) so the turn starts instead of waiting for ever.
* A finished turn, when Freebuff ends one. A turn can never hang for ever: if
  the transcript and the log stop moving for two minutes the turn is closed as
  an error that says so, and you can retry instead of watching a spinner. A TUI
  that is still drawing gets six minutes, because a long tool step can run
  without writing anything. When Freebuff prints its own "cannot reach the
  servers" wording the turn ends in about a second.
* Stop and resume: the chat id Freebuff writes is reported as `session_ref`,
  and resuming starts the CLI with `--continue <id>`.
* Interrupt: Escape, then Ctrl-C if the TUI is still working, then a turn that
  is honestly reported as interrupted.
* Shutdown: the CLI is terminated with the chat.

## The model catalog

`bin/windows-x64/freebuff-driver.exe --catalog` opens Freebuff's model picker,
reads every row it shows, writes `models.json` next to the driver and prints the
same JSON as one line:

```json
{"captured_at":1793650406878,"default":"DeepSeek V4.1 Flash 10/17 • high","models":[{"model":"DeepSeek V4.1 Flash 10/17 • high","display_name":"DeepSeek V4.1 Flash 10/17 • high","detail":"3x · May use data for AI training","is_default":true}]}
```

The daemon reads that file on its next probe and shows those models. A chat that
starts on a chosen model reads the picker anyway, so it also rewrites the file
from the screen it already has; that is the cheap refresh. The console's Refresh
button runs `--catalog` for a full capture, and it refuses while a chat on this
plugin is running, because the capture drives the same CLI in the same folder.

Two things worth knowing:

* The capture starts a throwaway chat inside `.catalog-cwd/` in the plugin
  folder, so the project folder it leaves behind always has that one name. It
  types nothing into it beyond the keys that walk the list and it closes the
  picker before the CLI is killed. Delete the folder if it ever bothers you;
  the next capture recreates it.
* A failed capture is still an answer: one JSON line with a `reason` sentence
  and exit code 0, and the console keeps the manifest's list.

## What a Freebuff update can break

The picker is read from the screen and the chat is read from Freebuff's private
store, so both are measured behaviour rather than a published contract. When a
release moves something, this plugin needs an update and OpenRemote does not.
The fixable parts, in the order they show up:

* **The store moved.** `%USERPROFILE%\.config\manicode\projects\` is internal to
  the CLI. A moved transcript or log means no text events; a moved chat folder
  means `--continue` no longer takes the id the driver reported.
* **The picker's wording changed.** A new marker, a renamed cost line or a new
  section header can make a row stop looking like a card. The driver then
  refuses with `Freebuff's model picker could not be read` rather than walking a
  screen it did not understand, and the chat still starts on Freebuff's own
  model.
* **A screen sequence the driver's grid does not implement.** The grid marks the
  screen unreadable until it is repainted, and an unreadable screen gets no
  keys. That is why a new spinner or graphic can turn a working switch into a
  refusal instead of a wrong pick.
* **A new model.** Refresh catches it with no plugin update, because the list
  comes from the picker. A model that was removed is caught the same way: a
  stale name is refused in a sentence naming it, never rounded to the nearest
  row.

## What is honestly unsupported

These are declared as gaps rather than faked, because the CLI gives no way to
know them:

* **No effort, no fast mode, no permission mode.** The model is the only setting
  a running chat can change; anything else is refused in a sentence rather than
  given an invented word.
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

## The model Freebuff runs is borrowed state

The model is not the chat's alone: Freebuff keeps it outside the transcript, and
picking a model in one chat is meant to be what the next chat starts on. The
driver therefore treats it as state it borrows rather than owns. Every spawn
reads the picker's marked row first and reports it in `initialized`; a chat is
moved only when the model it was asked for differs from that row. A resumed chat
re-applies the model the chat stored, through the same verified switch, so
resuming never silently changes what a chat runs. There is no restore on
shutdown: that would be a second TUI drive inside OpenRemote's three-second stop
deadline, and a resume already puts the chat back on its own model.

Where Freebuff keeps that state, and whether a driver-made pick carries to the
next CLI process, is measured behaviour rather than a promise. On the build this
was tested on, a switch the driver made was verified within that chat's process
(the reopened picker marked the new model) but the next process opened its
picker on the model from before. The chat is right either way, because its
`initialized` names what its own picker showed; a new chat's starting model may
not follow the driver's last switch.

## Build

```sh
cd driver
cargo build --release
# the binary is at target/release/freebuff-driver.exe
cp target/release/freebuff-driver.exe ../bin/windows-x64/freebuff-driver.exe
# read the model picker once, writing models.json beside the driver
../bin/windows-x64/freebuff-driver.exe --catalog
```

Gates, all green for the committed tree:

```sh
cargo test                      # the parsers, the screen grid and the event shapes
cargo clippy --all-targets -- -D warnings
cargo fmt --all --check
```

If your system drive is short of space, point the build somewhere else:
`CARGO_TARGET_DIR=D:\cargo\freebuff-driver cargo build --release`.

## What has been verified, and what has not

Verified on the machine this was built on, against Freebuff 0.2.35:

* `--probe` reporting `{"available":true,"reason":null,"signed_in":true,"version":"0.2.35"}`.
* `--catalog` reading the picker: 11 models with their cost lines, which are
  what the manifest ships. A later capture on the same machine read 12, because
  Freebuff had added a model since the committed capture; that is the refresh
  working, not a stale manifest.
* A pseudo terminal running the Freebuff TUI: it paints, stays alive, accepts a
  bracketed paste, and answers `-v` with `0.2.35` and exit code 0.
* A chat started on a chosen model: the driver read the marked row, walked the
  picker with the arrow keys, selected the row and proved it by reopening the
  list, and `initialized` named the verified model.
* The same through the daemon's own HTTP API, which is what the console does:
  the harness row carried the catalog and its capture time, the chat on a chosen
  model showed the model the driver verified, and the turn ended `finished`
  with the model's answer.
* The catalog refresh: refused while a chat was alive and accepted after it
  stopped, with the capture time moving forward.

Not verified in this round: a resume (`--continue`), and a turn long enough to
stream several tool calls.

One race worth knowing: Freebuff writes the reply into its transcript a moment
after it writes the turn's end marker, and the driver reads the transcript once
more when the marker lands. On one of two runs here that flush came after the
read, so the turn was reported `finished` with the reply missing from the chat.
Freebuff's own transcript held the answer both times, so the reply was not lost;
the chat simply did not show it for that turn.

## License

Not set yet: the repository ships no license file, and that is a decision for
its owner. OpenRemote itself is AGPL-3.0. Harness names and marks belong to
their owners; this plugin is not affiliated with Freebuff.
