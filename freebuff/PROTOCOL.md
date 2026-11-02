# PROTOCOL.md - the driver protocol this plugin speaks

This is the contract between OpenRemote and `freebuff-driver.exe`. It is
written so that a third party can write their own harness plugin against it
without reading OpenRemote's source, and so that this plugin's own behaviour is
checkable line by line.

Two versions are in play and they are different things:

* `openremote.json` `api` - the plugin manifest format. This plugin declares
  `1`.
* `harness.api` - the harness driver protocol. This plugin speaks `1`.

A daemon that does not know `harness.api` 1 lists the plugin and refuses to
offer its harness; it must not guess.

## 1. How the driver is started

The daemon runs the driver named by the manifest's `driver` map for its own
platform (`windows`, `macos`, `linux`, falling back to `default`), with the
plugin folder as the working directory and the manifest's `args` appended. The
resolved path stays inside the plugin folder; a path that escapes it is
refused.

The driver is a console-less child. It must not open a window, and it must not
read the daemon's own secrets: it inherits the user's environment, nothing more.

## 2. Three modes

### `--probe`

One JSON line on stdout, then exit 0. Always exactly one line, and always the
same four keys:

```json
{"available":true,"reason":null,"signed_in":true,"version":"0.2.35"}
{"available":false,"reason":"Freebuff CLI not found at C:\\Users\\you\\.config\\manicode\\freebuff.exe","signed_in":null,"version":null}
```

| key | type | meaning |
|-----|------|---------|
| `available` | bool | the CLI is present and this driver can drive it |
| `version` | string or null | the CLI's version, as the CLI reports it |
| `signed_in` | bool or null | `null` when the driver cannot tell |
| `reason` | string or null | one plain sentence for a person, `null` when available |

Rules the console relies on:

* An unavailable CLI is an answer, not a failure: exit 0, `available: false`.
* No guessing. A version is reported only when the CLI printed one; a sign-in
  state only when the driver can actually see it.
* `reason` is shown verbatim. Keep it one sentence, in plain language, and
  never put a token, a key or an account address in it.

### `--catalog`

Opens Freebuff's model picker, reads every row it shows, writes `models.json`
into its working directory (the plugin folder), and prints the same JSON as one
line on stdout. Always exit 0, for the same reason as `--probe`: a picker that
could not be read is an answer, not a driver failure.

```json
{"captured_at":1793650406878,"default":"DeepSeek V4.1 Flash 10/17 • high","models":[{"model":"DeepSeek V4.1 Flash 10/17 • high","display_name":"DeepSeek V4.1 Flash 10/17 • high","detail":"3x · May use data for AI training","is_default":true}]}
```

| key | type | meaning |
|-----|------|---------|
| `captured_at` | number | when the picker was read, milliseconds since the epoch; `0` when nothing was read |
| `default` | string or null | the row the picker had marked before any arrow, which is the model Freebuff runs now |
| `models` | array | the picker's cards in the order it shows them, in Freebuff's own words |
| `reason` | string or null | present instead of an answer when the read failed: one plain sentence |

Each model carries `model`, `display_name`, `detail` (the row's cost line,
verbatim), `note` (the data-use or plan notice printed under a name) and
`is_default`. That is the daemon's own model descriptor, so `models.json` parses
as the catalog the console shows, and the fields the daemon has and Freebuff does
not are simply absent.

Rules this mode keeps:

* It is the only mode that opens the picker purely to read it. It starts a
  throwaway chat inside the plugin folder's `.catalog-cwd/`, so the project
  folder that chat lands in always has the same name, and it types nothing into
  it beyond the keys that walk the list.
* It never presses a key on a screen it has not read. `escape` is the only key
  sent after the last parsed picker, and it is sent whether the read worked or
  not, so a half-open list is never left for the next chat.
* A failed write of `models.json` is not a failure of the answer: the daemon
  falls back to the manifest's shipped list when the file is missing or
  unreadable.

### Session mode

With no arguments the driver reads commands on stdin and writes events on
stdout, one JSON object per line, until it is told to shut down or its child
exits.

* stdout carries events and nothing else. One line, one JSON object.
* stderr carries diagnostics: human-readable lines, free-form.
* A malformed input line is a protocol error for that line only. The driver
  reports it on stderr and keeps the session alive.
* Unknown command types and unknown fields are ignored, so a newer daemon does
  not break an older driver and the other way round.
* Additions are additive. Event and command types may be added; a field may be
  added to an existing message. Neither is a breaking change.

## 3. Commands: daemon to driver

| `type` | fields | meaning |
|--------|--------|---------|
| `spawn` | `cwd`, `model`, `permission_mode`, `effort`, `resume`, `fast` | start the harness process for this chat |
| `prompt` | `text` | send one user prompt |
| `interrupt` | none | cancel the open turn |
| `answer` | `harness_ref`, `choice` | answer an approval request |
| `settings` | `model`, `effort`, `fast`, `permission_mode` | change the model in the running chat, or refuse the rest |
| `shutdown` | none | end the driver and its harness process |

`spawn` is the first command and comes once. `cwd` is the chat's working
directory; it must exist. `resume` is a `session_ref` this driver reported
earlier, or absent for a new chat. `model`, `effort`, `fast`,
`permission_mode` are optional: `null` or absent means the daemon has no
preference.

`model` is a name from this plugin's catalog (`--catalog`), a picker row in
Freebuff's own words. It is matched exactly after trimming, never by nearest
row: a name that differs in case or punctuation is a refusal, not a guess.
`effort`, `fast` and `permission_mode` have no word in Freebuff; a `settings`
command carrying one is refused in a sentence rather than given an invented
value.

## 4. Events: driver to daemon

| `type` | fields |
|--------|--------|
| `initialized` | `session_ref`, `model`, `permission_mode` |
| `model_reported` | `model` |
| `settings_refused` | `message` |
| `text`, `text_delta` | `text` |
| `reasoning`, `reasoning_delta` | `text` |
| `tool_started` | `tool_use_id`, `name`, `input` |
| `tool_result` | `tool_use_id`, `name`, `text`, `is_error` |
| `approval_requested` | `harness_ref`, `request`, `kind`, `options`, `tool_name`, `summary` |
| `turn_completed` | `subtype`, `coarse`, `is_error`, `error_message` |
| `context_used` | `used`, `window` |
| `turn_cost` | `cost_usd` |
| `stderr` | `line` |
| `closed` | none |

Two rules that matter more than the table:

1. **`closed` is terminal.** It means the harness process is gone and the
   driver will not produce another event. Emit it once.
2. **A turn is reported exactly once.** `turn_completed` ends the open turn and
   only then may the next `prompt` be answered.
3. **`model_reported` is the screen's own answer.** It carries the model the
   driver read off Freebuff's picker, so a chat's chip follows what the TUI
   actually runs. `settings_refused` carries one sentence saying why a change
   did not happen, and the daemon shows it as a note in the chat.

## 5. What this driver does with each command

### `spawn`

1. Resolve the Freebuff CLI: `%USERPROFILE%\.config\manicode\freebuff.exe`,
   then `$HOME/.config/manicode/freebuff`, then `freebuff` on `PATH`. The npm
   `freebuff.cmd` shim is never used: its launcher runs an update check that
   waits on a TTY, so a console-less child hangs on it.
2. Run `freebuff -v`. A version older than 0.2.35 is refused with a message,
   because the chat store this driver reads is internal to the CLI.
3. Check that `credentials.json` exists next to the CLI. It is never read.
   Missing means "run `freebuff login` once in a terminal", and no session
   starts.
4. Start `freebuff --cwd <cwd> [--continue <resume>]` in a 140x45 pseudo
   terminal, `TERM=xterm-256color`, with the chat's working directory. The CLI
   takes no prompt argument, so the prompt arrives later, as input.
5. Wait for the TUI to paint and then be quiet for 800 ms (bounded at 8 s) and
   for Freebuff's chat directory to appear (bounded at 20 s).
6. When `model` is present, put the chat on it. The driver types `/model` and
   Enter, reads the row the picker marks, and compares it with `model`: equal
   means no arrow is pressed at all. Otherwise it walks with Down, reading the
   picker after each press, until the marked row is the wanted name; then Enter
   selects it and the driver reopens the picker to prove the mark landed there.
   Nothing here types a prompt and nothing answers a spend dialog: the only
   keys are `/model`, Enter, Down and Escape. The rules that bound the walk:
   * **No keystroke without a parsed screen.** A key is sent only after the
     screen has been read and is known not to be the picker when Enter would
     mean "select". A startup notice, a survey or an ad can swallow the first
     `/model`, so opening the picker is attempted twice, then refused in a
     sentence.
   * **The picker is a structural fact.** It is a screen with at least two
     distinct model cards and a marked row; a single card, a transcript or a
     survey is never read as one. A screen carrying an escape sequence this
     driver's grid does not implement is unreadable until it is repainted, and
     unreadable means no keys.
   * **Exact match, no nearest row.** The wanted name must equal the marked
     row after trimming, character for character. The walk is bounded at 240
     presses and also stops when the mark has not moved for two presses, which
     is how the bottom of the wrapped list is found.
7. Emit `initialized` once, with `session_ref` set to the chat directory's own
   name, which is what `--continue` takes.

`model` in `initialized` is the model the picker proved when a switch was
verified, the requested one when the picker could not be read, and `null` when
`spawn` named none. `permission_mode` is echoed from `spawn` unchanged.

A switch that fails does not end the chat. It stays alive on the model Freebuff
runs, `initialized` names the model the picker showed (the requested name when
the picker could not be read at all), and the first `prompt` is refused with a
`turn_completed` error naming both the wanted and the actual model, so nothing
is spent on a model the person did not choose. A chat that was put right by a
later settings change is not stuck behind that refusal.

Freebuff remembers the last model chosen for the machine, not for the chat. A
spawn whose model already is that one presses no arrow, and a resumed chat
re-applies the model it stored by the same verified switch.

### `prompt`

The text is typed into the TUI as a bracketed paste, `ESC[200~`, the text,
`ESC[201~`, then Enter. Freebuff then works as it normally would.

Freebuff can answer that Enter with its own cost confirmation: the message sits
in the composer with `Your message is saved.` and `Enter: confirm and send`, and
nothing starts until a second Enter arrives. The driver presses it, because the
person already sent the message and that Enter is the same key they would press.
The count is bounded at 3 per turn, paced at 3 s apart, and it only ever reads
the part of the screen a person can currently see - a dialog left in the
scrollback is never answered again.

The driver does not scrape the screen for answers. It reads what Freebuff
writes into its own chat store:

* `chat-messages.json` is polled every 250 ms. Text blocks become `text` (first
  sight) or `text_delta` (the block grew); blocks marked as reasoning become
  `reasoning` / `reasoning_delta`; a tool block becomes `tool_started` when it
  appears and `tool_result` when its `output` lands; a `mode-divider` block is
  ignored, and so are the user's own messages.
* `log.jsonl` is tailed from its end. A line whose `msg` is
  `Main prompt finished` ends the turn.

One prompt at a time: a `prompt` while a turn is open is refused on stderr and
is not queued.

### `interrupt`

Escape is sent to the TUI. If it is still painting after 1.2 s, Ctrl-C is sent
as well. The driver then waits up to 5 s for the turn's end, and reports:

```json
{"type":"turn_completed","subtype":"interrupted","coarse":"interrupted","is_error":false}
```

That is the only end this driver ever synthesizes, and it never claims a turn
succeeded. A turn that did not end and was not interrupted is reported as an
error, not as `finished`.

### `settings`

The model can be changed on a running chat, between turns. The driver opens
`/model`, walks to the name and proves it by reopening the picker, exactly as
at spawn, and the change takes a few seconds because every key waits for the
TUI to settle. Success is reported as `model_reported` with the model the
picker proved, which is the truth the console's chip follows.

The rest is refused, on stderr and as a `stderr` event, and where the daemon can
show it also as `settings_refused` with one sentence:

* `effort`, `fast` or `permission_mode` present: Freebuff has no such word, and
  the driver will not invent one.
* a turn in flight: the picker would swallow that turn's keystrokes, so the
  change waits for the turn to land. The driver never opens the list under a
  running turn.

A change the driver could not make keeps the chat on the model it had: it emits
`model_reported` with the model the picker still marks, when that was read, then
`settings_refused` naming both. The chat keeps running and the next prompt is
not blocked by it.

### `answer`

Refused, on stderr and as a `stderr` event. This harness asks no approvals, so
there is nothing to answer, and the driver never invents a decision.

### `shutdown`

Freebuff is terminated, `closed` is emitted, and the driver exits 0.

## 6. Turn ends

| what the log said | event |
|-------------------|-------|
| `Main prompt finished` | `{"type":"turn_completed","subtype":"finished","coarse":"completed","is_error":false}` |
| a line with `Validation failed` or `fetch failed` | `{"type":"turn_completed","subtype":"error","coarse":"failed","is_error":true,"error_message":"<the msg field, redacted>"}` |
| `interrupt` given by the daemon | `{"type":"turn_completed","subtype":"interrupted","coarse":"interrupted","is_error":false}` |
| no transcript or log progress for 120 s, screen frozen | `{"type":"turn_completed","subtype":"error","coarse":"failed","is_error":true,"error_message":"Freebuff reported nothing for 120 seconds; the turn was closed as stalled"}` |
| no progress for 360 s, screen still printing | the same stalled end, naming 360 |
| Freebuff's own "cannot reach the servers" wording on screen | `{"type":"turn_completed","subtype":"error","coarse":"failed","is_error":true,"error_message":"Can't reach Freebuff's servers. Retrying… Try another network."}` |
| Freebuff exits with a turn open | `{"type":"turn_completed","subtype":"error","coarse":"failed","is_error":true,"error_message":"Freebuff exited before the turn finished"}`, then `closed` |
| a `prompt` after a model switch failed at spawn | `{"type":"turn_completed","subtype":"error","coarse":"failed","is_error":true,"error_message":"Freebuff did not switch to <wanted>: it is still on <actual>, so the prompt was not sent. Pick another model, then send it again."}` |

A turn is never left open indefinitely. The chat store this driver reads belongs
to the CLI, so it can move or stop being written while the CLI is still running;
without the stalled-turn end above, such a turn would sit "working" in the
console for ever with nothing to explain it. The stall end says the outcome is
unknown and lets the user retry, which is honest where a spinner is not.

Two rules keep that end from firing on honest work or arriving too late:

* A TUI that has printed within the last 15 seconds is still alive and may be
  inside one long tool step that writes nothing to the transcript, so it earns
  three times the window (360 s). A frozen screen gets 120 s.
* The screen is checked every second for Freebuff's own unreachable-servers
  wording, so that banner ends the turn in about a second rather than waiting
  for any window.

`FREEBUFF_DRIVER_TURN_LIMIT_SECS` changes the 120-second window (the live-screen
window scales with it); a value under 5 is ignored.

`error_message` is the log line's `msg` field, with anything address- or
token-shaped replaced by `<redacted>` and the text clipped. Freebuff's log lines
carry the account's id and address on many lines; none of that is ever passed
on.

## 7. What this driver never reports

* **Approvals.** No `approval_requested` is ever emitted, because Freebuff has
  no approval flow this driver can see. If the TUI ever stops on a question,
  the driver reports it as a diagnostic with the screen text and does not
  answer it.
* **Context and cost.** No `context_used` and no `turn_cost`. Freebuff reports
  neither to a client.
* **Any `text_delta` promise.** Whether Freebuff updates `chat-messages.json`
  during a turn or only at the end is measured behavior, not a contract. When
  it updates at the end, the answer arrives as one `text` event.
* **A model nobody could read.** A model is reported only after the picker
  showed it, and a model name is never matched by similarity: an unknown,
  mistyped or retired name is a refusal that says so, never the nearest row.

The catalog is the one measured model list, and it is measured by `--catalog`
rather than on every spawn. A spawn that reads the picker for a model choice
also rewrites `models.json` from the screen it already has, which is a free
refresh; a catalog capture on its own is an explicit act, run by the console's
Refresh and refused while a chat on this plugin is alive.

## 8. Diagnostics

`stderr` events carry anything a person should see in the console: the version
it resolved, the chat id it found, why a turn failed, and, every 30 seconds of
a turn with no transcript progress, the last readable lines of the TUI. That
last one exists because Freebuff can stop on something this driver cannot
answer; quoting the screen is honest, guessing is not.

**Screen text goes to stderr only.** A TUI frame can carry the signed-in
address, a balance or an account id, so a frame is never put in an event field:
not in `error_message`, not in `settings_refused`, and not in a note the daemon
would sync to another device. When a model switch fails, the driver quotes the
last frame on stderr and the event carries a redacted sentence naming the two
model names and nothing else. A model name is safe in an event for the same
reason it is safe in the manifest: it is the harness's own public vocabulary,
read off its picker.

`FREEBUFF_DRIVER_ACTIVITY_NOTE_SECS` shortens that 30-second window while you
are developing a driver of your own. It changes no event.

## 9. The Freebuff chat store this driver reads

This is what was measured on Freebuff CLI 0.2.35. It is internal to the CLI and
may move in any release; when it does, this plugin needs an update, not
OpenRemote.

```
~/.config/manicode/
  freebuff.exe                  the CLI
  credentials.json              presence only, never read
  projects/<project>/chats/<chat-id>/
      chat-messages.json        the transcript
      log.jsonl                 the CLI's own log, with the turn marker
      chat-meta.json            counts and the first prompt
      run-state.json            the CLI's internal state
```

* `<project>` is the working directory's own name.
* `<chat-id>` is a timestamp such as `2026-10-31T20-33-15.026Z`, and it is
  exactly what `--continue` takes.

`chat-messages.json` is an array of messages:

```json
[
  {"id":"divider-1","variant":"ai","content":"","blocks":[{"type":"mode-divider","mode":"LITE"}]},
  {"id":"user-1","variant":"user","content":"the prompt"},
  {"id":"ai-1","variant":"ai","content":"","isComplete":true,"blocks":[
    {"type":"text","content":"thinking","textType":"reasoning","thinkingId":"thinking-1"},
    {"type":"tool","toolCallId":"hx..","toolName":"run_terminal_command",
     "input":{"command":"ls"},"output":"file.txt\n"},
    {"type":"text","content":"Done.","textType":"text"}
  ]}
]
```

A tool block has no `output` until the result lands; the driver treats that
first appearance as the tool's result. `log.jsonl` is one JSON object per line
and the end of a turn is:

```json
{"level":"DEBUG","timestamp":"...","data":{"outputType":"lastMessage","messageCount":1},"msg":"Main prompt finished"}
```

## 10. Pseudo terminal notes for the next author

A TUI harness on Windows has two traps, both found the hard way while building
this plugin. They are not Freebuff-specific.

1. **Keep the `portable_pty` master alive for the whole session.** The ConPTY
   handle lives inside an `Arc<Mutex<Inner>>` shared by the master and the
   slave. Dropping both closes the pseudo console, and the child dies at once
   with exit code `0xC0000142` (`STATUS_DLL_INIT_FAILED`) having printed
   nothing. It looks exactly like "ConPTY cannot drive this binary".
2. **Answer the cursor-position query.** `CreatePseudoConsole` is called with
   `INHERIT_CURSOR`, so conhost sends `ESC[6n` to the client and blocks the
   child's console init until it gets a cursor report. Write `ESC[1;1R` back.
   Without it, `cmd.exe`, `node` and `freebuff` all hang with no output at all.

## 11. Worked example

A session that starts a chat, asks one question and stops:

```text
-> {"type":"spawn","cwd":"C:\\work\\demo","resume":null}
<- {"type":"stderr","line":"Freebuff 0.2.35 at C:\\Users\\you\\.config\\manicode\\freebuff.exe"}
<- {"type":"stderr","line":"started Freebuff (pid 12345) in C:\\work\\demo at 140x45"}
<- {"type":"stderr","line":"chat 2026-10-31T20-33-15.026Z"}
<- {"type":"initialized","session_ref":"2026-10-31T20-33-15.026Z","model":null,"permission_mode":null}
<- {"type":"stderr","line":"the Freebuff TUI painted and went quiet"}
-> {"type":"prompt","text":"read app.py and add type hints"}
<- {"type":"reasoning","text":"I should read the file first."}
<- {"type":"tool_started","tool_use_id":"hxvAWKMbGo0","name":"read_files","input":{"path":"app.py"}}
<- {"type":"tool_result","tool_use_id":"hxvAWKMbGo0","name":"read_files","text":"def add(a, b):\n","is_error":false}
<- {"type":"text","text":"Added type hints to app.py."}
<- {"type":"turn_completed","subtype":"finished","coarse":"completed","is_error":false}
-> {"type":"shutdown"}
<- {"type":"closed"}
```

The `tool_started` and `tool_result` names are Freebuff's own tool names, passed
through unchanged, because harness vocabulary belongs to the harness.
