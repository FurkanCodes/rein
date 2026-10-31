# rein

A tiny, fast agent harness. You hold the reins.

rein is an open-source agent harness written in Rust, built to be the
fastest and smallest harness around, and a smart one. It's early: today it can sign in and
stream an answer. The agent loop and tools come next.

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

## Layout

| Crate | What it is |
|---|---|
| `crates/rein-llm` | Streaming LLM provider layer: SSE parsing, the Responses API, Sign in with ChatGPT |
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
