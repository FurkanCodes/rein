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
  to your API account.

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

In a conversation, your message goes in a box pinned to the bottom of the
terminal, with a status bar under it: the model, the mode, how full the
context is, how much came from the cache, and how long the last turn took.
The conversation scrolls above it as usual, so you can scroll back and copy
from it. Each step rein takes appears under a label (`read`, `edit`, `run`),
with a short result and the output lines worth seeing; edits show as a
framed diff before you approve them. Answers are rendered from Markdown.
`NO_COLOR`, and output that isn't a terminal, get plain text.

| Key | What it does |
|---|---|
| ↑ / ↓ | your earlier messages |
| `\` at the end of a line, or Ctrl-J | a new line in your message |
| Ctrl-C while rein works | stop that turn; the conversation goes on |
| Ctrl-C at the prompt | clear the line; press again within two seconds to leave |
| `/compact [focus]` | make room now; the summary keeps `focus` in mind |
| Ctrl-D or `/exit` | leave |

Every conversation is saved as it goes, one JSON line per message, tool call
and result, in `~/.config/rein/sessions/<this folder>/`. The files are readable
only by you. `rein -c` picks up the latest one in the folder you're in, with
the same history, system prompt and cache key, so it usually starts warm.

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
removes and adds. At each question, answer `y`, `n`, or `a` for "yes, and
don't ask again this run".

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
so they need no network and spend no tokens. Record a real stream with
`rein --record crates/rein-llm/tests/fixtures/<name>.sse "<prompt>"`. Recordings
contain your account's `safety_identifier`; change it to `user-REDACTED` before
committing. A test fails if you forget.
