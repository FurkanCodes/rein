# rein

A tiny, fast agent harness. You hold the reins.

rein is an open-source agent harness written in Rust, built to be the
fastest and smallest harness around, and a smart one. It's early: today it holds
a conversation in the folder you run it in, reading, writing and editing files
and running commands, asking you first.

## Build

```sh
cargo build --release
./target/release/rein --help
```

## Paying for the model

rein uses OpenAI's Responses API and can pay for it in two ways:

- **Your ChatGPT plan.** Run `rein login` to continue with ChatGPT. This uses
  OpenAI's official [Sign in with ChatGPT](https://developers.openai.com/siwc/token-sharing-open-source)
  for open-source apps. Usage counts toward your plan's limits; on Plus, the
  five-hour limit is shared with every other app that uses your plan. Manage it at
  [ChatGPT settings → Usage](https://chatgpt.com/settings/usage).
- **An API key.** If you haven't signed in, rein uses `OPENAI_API_KEY`, billed
  to your API account. `OPENAI_BASE_URL` points it at another server that
  speaks the Responses API, like a proxy.

rein prints which one it's using on every run.

## Usage

```sh
rein login                      # continue with ChatGPT
rein models                     # list the models you can use
rein                            # a conversation in this folder
rein "explain what a harness is"   # one task, then exit
rein -c                         # continue the latest conversation here
rein -r                         # choose a saved conversation to continue
rein --model gpt-6.1-sol --effort low "say hi"
```

In a conversation rein takes the whole window, as `less` and `vim` do: the
conversation above, and your message in a box at the bottom, with a status
bar under it: the model and its effort, the mode, how full the context is,
how much came from the cache, and how long the last turn took. While rein
works, a line over the box says what it's doing and for how long. Each step
appears on one line once it's done (`Read`, `Edit`, `Run`), with a short
result and the output lines worth seeing; edits show as a framed diff
before you approve them. Answers are rendered from Markdown.

rein keeps the conversation as text and lays it out for the window's width,
so when the window changes size nothing is lost and words wrap where they
should. Scroll it with the mouse wheel, PgUp/PgDn or Shift-↑↓; drag over
text to copy it. What the terminal showed before rein is left as it was,
and when you leave, the conversation is printed there, so it stays in the
scrollback. `rein "task"` and output that isn't a terminal get plain lines
instead, printed as they come; `NO_COLOR` turns colour off.

When rein needs your go-ahead, the box turns yellow and asks, with the whole
command in view: **y** yes, **a** yes and don't ask again, **n** no, or
**Esc** no, and stop so you can say what to do instead. The arrows and Enter,
or 1–4, work too. Keys typed in the moment it appears still go to your
message, so typing ahead can't answer it by accident.

| Key | What it does |
|---|---|
| ↑ / ↓ | what you sent before from this folder, this run or an earlier one |
| Ctrl-R | find something you sent before by its words: ↑↓ or Ctrl-R pick, Enter puts it in the box to change or send, Esc goes back |
| Shift-Enter, Ctrl-J, or `\` at the end of a line | a new line in your message (Shift-Enter where the terminal tells it apart from Enter) |
| `/` | the commands that match, as you type: ↑↓ pick one, Tab completes it, Enter runs it. `/help` lists them |
| Shift-Tab | the next mode: ask → edits → read-only |
| Enter while rein works | queue your message; it's sent when rein is done. Esc puts it back in the box |
| Esc or Ctrl-C while rein works | stop that turn; the conversation goes on |
| Ctrl-C at the prompt | clear the line; press again within two seconds to leave |
| Alt-← / Alt-→ | a word left or right |
| Ctrl-O | see every step's full output: ↑↓ scroll, ←→ other steps, `/` search, Esc back |
| Mouse wheel, PgUp / PgDn, Shift-↑ / ↓ | scroll the conversation |
| Drag over text | select it and copy it to the clipboard |
| Ctrl-L | draw the window again |
| `/mode [ask\|edits\|read-only\|trust]` | show or change what runs without asking (also `/permissions`) |
| `/model [id]`, `/effort [level]` | list or switch the model; show or change reasoning effort |
| `/undo` | put back the files rein changed in its last turn (not what commands did) |
| `/new`, `/resume`, `/status` | a fresh conversation (also `/clear`); continue a saved one; what's in use |
| `/compact [focus]` | make room now; the summary keeps `focus` in mind |
| `/copy` | copy rein's last answer as it wrote it, Markdown and all |
| Ctrl-D or `/exit` | leave |

A big paste shows in the box as one piece, `[pasted 40 lines]`, which
Backspace takes out whole; the model gets all of it.

Every conversation is saved as it goes, one JSON line per message, tool call
and result, in `~/.config/rein/sessions/<this folder>/`. The files are readable
only by you. `rein -c` picks up the latest one in the folder you're in, with
the same history, system prompt and cache key, so it usually starts warm.

What you send is also kept on its own, for ↑ and Ctrl-R: the last 1,000
messages from each folder, each once, in `~/.config/rein/history/`, readable
only by you. A big paste comes back as the piece it was.

rein rings the terminal's bell when it needs you and you're away: it's asking
something, or it's done. Away means its window isn't in front, which most
terminals report when asked (tmux does with `set -g focus-events on`), or,
where the terminal doesn't say, that you haven't touched a key for 10
seconds. What a bell does is the terminal's to decide: a sound, a flash, or a
mark on the tab or dock icon. `bell = off` in the settings turns it off.

## When the conversation gets long

Every request carries the whole conversation, so the usage line after each
one also says how full it is: `context 42k/150k`. Past the limit (150,000
tokens, or `$REIN_COMPACT_AT`, and never more than 90% of the model's window)
rein makes room, cheapest first:

1. **Stale tool output is retired:** an old read of a file that was read or
   changed again later, and command output from before your last two
   messages. Each becomes one line saying what was there. When that's enough,
   nothing else happens.
2. **Otherwise the model writes a summary.** It's asked with the conversation
   exactly as it was plus one more message, so the request is mostly served
   from the prompt cache. rein adds its own record of your messages, the
   files changed and the commands run, so those facts can't be misremembered.
   The conversation then continues from the first message, the summary, and
   its latest part word for word.

Nothing is lost: the session file keeps everything, and the model's `recall`
tool searches the whole conversation, including what was compacted away.

## Settings

Defaults go in `~/.config/rein/config`, one per line:

```text
model = "gpt-6.1-sol"
effort = "medium"
mode = "edits"
compact_at = 120000
bell = off
```

A flag wins over `$REIN_MODEL`, `$REIN_EFFORT`, `$REIN_MODE`,
`$REIN_COMPACT_AT` and `$REIN_BELL`, which win over the file.

## Small things that matter

- A request that fails before anything streams (network trouble, a busy
  server, a rate limit) is retried, 1, 2 and 4 seconds apart.
- Commands are stopped after 60 seconds; the model can ask for up to 10
  minutes for a slow build or test suite.
- For work with several steps the model keeps a plan, shown as a checklist.
- What the model thought about shows as one dim line: `✻ Checking the tests`.

## What rein tells the model

- **A short system prompt** of its own (`crates/rein-agent/src/prompt.rs`), with
  nothing in it that changes over time. `-s "..."` adds your standing
  instructions to it.
- **A first message** with the working folder, the operating system, today's
  date (no time of day) and the project's instructions: `AGENTS.md` files from
  the repository root down to the folder you're in (`CLAUDE.md` where a folder
  has none), plus your own `~/.config/rein/AGENTS.md`, 32 KB at most. It's
  saved with the conversation, so a continued one starts with the same text.

Credentials live in `~/.config/rein/` (or `$REIN_HOME`), readable only by you.

## What rein may do

rein's tools are `read`, `write`, `edit` and `bash`. They only touch files
inside the folder you run it in. Before a file change you see the lines it
removes and adds. At each question, answer `y`, `n`, `a` for "yes, and
don't ask again this run", or Esc to say no and tell rein what to do instead.

`--mode` sets what runs without asking:

| Mode | File changes | Commands |
|---|---|---|
| `ask` (default) | ask | ask |
| `edits` | allowed | ask |
| `read-only` | refused | refused |
| `trust` | allowed | allowed, except deny rules |

Rules in `.rein/permissions` refine this for commands:

```
allow cargo test
ask   git commit
deny  git push
```

Deny beats ask beats allow. A command runs without asking only if every part
of it is allowed, so `allow cargo test` doesn't cover `cargo test && rm -rf x`.
Anything rein can't take apart, like `$(...)`, redirects or globs, is asked
about. Rules decide when rein asks; they aren't a sandbox, so `deny rm`
doesn't stop `/bin/rm`.

## Layout

| Crate | What it is |
|---|---|
| `crates/rein-llm` | Streaming LLM provider layer: SSE parsing, the Responses API, Sign in with ChatGPT |
| `crates/rein-agent` | The agent loop, the tools, the permission engine, saved conversations, and what the model is told first |
| `crates/rein-cli` | The `rein` command |

## Tests

```sh
cargo test
```

The tests replay saved response streams from `crates/rein-llm/tests/fixtures`,
so they need no network and spend no tokens. `crates/rein-cli/tests/screen.rs`
runs the real `rein` in a pseudo-terminal, against a model that follows a
script, and reads the screen through a terminal emulator kept in memory: it
types, scrolls, approves commands, resizes the window, leaves and comes back,
and checks what you would see, and when the bell rang and what was copied.
Record a real stream with
`rein --record crates/rein-llm/tests/fixtures/<name>.sse "<prompt>"`. Recordings
contain your account's `safety_identifier`; change it to `user-REDACTED` before
committing. A test fails if you forget.
