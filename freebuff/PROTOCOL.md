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

## 2. Two modes

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
| `settings` | `model`, `effort`, `fast`, `permission_mode` | change a setting, or refuse |
| `shutdown` | none | end the driver and its harness process |

`spawn` is the first command and comes once. `cwd` is the chat's working
directory; it must exist. `resume` is a `session_ref` this driver reported
earlier, or absent for a new chat. `model`, `effort`, `fast`,
`permission_mode` are optional: `null` or absent means the daemon has no
preference. This harness advertises no models, so in practice they are absent.

## 4. Events: driver to daemon

| `type` | fields |
|--------|--------|
| `initialized` | `session_ref`, `model`, `permission_mode` |
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
6. Emit `initialized` once, with `session_ref` set to the chat directory's own
   name, which is what `--continue` takes.

`model` is always `null` in `initialized`: the CLI has no way to report the
model it is using, and the manifest advertises none. `permission_mode` is
echoed from `spawn` unchanged.

### `prompt`

The text is typed into the TUI as a bracketed paste, `ESC[200~`, the text,
`ESC[201~`, then Enter. Freebuff then works as it normally would.

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

Refused, on stderr and as a `stderr` event. A running Freebuff chat cannot
switch model, effort, fast mode or permission mode. Stop the chat and resume it.

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
| Freebuff exits with a turn open | `{"type":"turn_completed","subtype":"error","coarse":"failed","is_error":true,"error_message":"Freebuff exited before the turn finished"}`, then `closed` |

`error_message` is the log line's `msg` field, with anything address- or
token-shaped replaced by `<redacted>` and the text clipped. Freebuff's log lines
carry the account's id and address on many lines; none of that is ever passed
on.

## 7. What this driver never reports

* **Models.** No `models` in the manifest, `model: null` in `initialized`. The
  console hides its model picker because the manifest declares none.
* **Approvals.** No `approval_requested` is ever emitted, because Freebuff has
  no approval flow this driver can see. If the TUI ever stops on a question,
  the driver reports it as a diagnostic with the screen text and does not
  answer it.
* **Context and cost.** No `context_used` and no `turn_cost`. Freebuff reports
  neither to a client.
* **Any `text_delta` promise.** Whether Freebuff updates `chat-messages.json`
  during a turn or only at the end is measured behavior, not a contract. When
  it updates at the end, the answer arrives as one `text` event.

## 8. Diagnostics

`stderr` events carry anything a person should see in the console: the version
it resolved, the chat id it found, why a turn failed, and, every 30 seconds of
a turn with no transcript progress, the last readable lines of the TUI. That
last one exists because Freebuff can stop on something this driver cannot
answer; quoting the screen is honest, guessing is not.

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
