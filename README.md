# rein

A tiny, fast agent harness. You hold the reins.

rein is an open-source agent harness written in Rust, built to be the
fastest and smallest harness around, and a smart one. It's early: today it can
read, write and edit files in the folder you run it in, and run commands, asking
you first. Each run is one task; conversations you can continue come next.

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
rein "explain what a harness is"
rein --model gpt-6.1-sol --effort low "say hi"
```

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
| `crates/rein-agent` | The agent loop, the tools, and the permission engine |
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
