# 01: Talking to a model

**Code:** `git checkout ep01`

rein sends a message to a model and streams the answer back:

```
$ rein "Say exactly: Hello, world!"
Using ChatGPT plan (manage usage: https://chatgpt.com/settings/usage)
Hello, world!
[13 in (0 cached), 8 out (0 reasoning)]
```

That's not an agent yet. But every harness (Claude Code, Codex, pi) starts
here: send text to a model, get text back.

## The idea: the model remembers nothing

Think of a model as a function: text in, text out. It can't see your files,
can't run commands, and keeps nothing between requests. A **harness** is
everything around it: the conversation history, the tools, the permission
checks, and the interface you type into. Anthropic uses the same word for
[the agent harness that powers Claude Code](https://claude.com/blog/building-agents-with-the-claude-agent-sdk),
and [pi](https://pi.dev) calls itself "a minimal agent harness".

Because the model remembers nothing, the harness **resends the whole
conversation on every request**. Anthropic's docs put it plainly: "The
Messages API is stateless, which means that you always send the full
conversational history." OpenAI's Responses API can store a conversation for
you, but rein sends `store: false` and keeps the history itself. (That flag
only stops OpenAI from saving the response for later; its
[abuse-monitoring logs](https://developers.openai.com/api/docs/guides/your-data)
are kept for up to 30 days either way.)

## What we built

```
┌──────────── rein ────────────┐
│ UI        rein-cli            │  ← basic, this episode
│ Loop      ask → act → repeat  │  ← next episode
│ Tools     read, bash, edit    │
│ Context   what to resend      │
│ Provider  rein-llm            │  ← this episode
└──────────────┬───────────────┘
               │ HTTPS + server-sent events
               ▼
          the model
```

| Piece | File | Why a harness needs it |
|---|---|---|
| SSE parser | `crates/rein-llm/src/sse.rs` | Answers stream in as [server-sent events](https://html.spec.whatwg.org/multipage/server-sent-events.html): a few lines each, ended by a blank line. The network splits them anywhere, even inside an emoji's four bytes, so the parser buffers bytes until it has whole lines. |
| rein's own events | `Event` in `lib.rs` | OpenAI's streaming API defines dozens of event types (59 in its Python SDK). rein turns them into a few of its own (`TextDelta`, `Completed`), so the rest of the harness never depends on one provider's format. Adding Anthropic later means writing one more decoder; pi's `pi-ai` package does the same job. |
| Exact endings | `EventDecoder` in `openai.rs` | A turn only counts as done on `response.completed`. A dropped connection is `Interrupted`, never "done", so the agent can't act on half an answer. |
| Sign in | `openai/signin.rs` | OpenAI's official [Sign in with ChatGPT for open-source agents](https://developers.openai.com/siwc/token-sharing-open-source), or an `OPENAI_API_KEY`. |
| Private storage | `store.rs` | Credentials are written atomically, readable only by you (`0600`). |
| Replay tests | `tests/replay.rs` | The model is random; the harness shouldn't be. We record real answers once and test against them forever: no network, no tokens. The same stream is also fed 1, 2, 3, 7 and 64 bytes at a time and must give the same answer. |

## Decisions

- **Rust.** Hello-world binaries measured on a Mac: Bun 55 MB, Go 5.4 MB,
  Rust 279 KB. rein's release binary, including TLS, HTTP, JSON and sign-in,
  is **2.3 MB** and starts in about **2 ms** (median of 50 runs of
  `rein --help`).
- **Stateless on purpose.** rein holds the conversation itself instead of
  relying on server-side state, so the same loop will work with providers
  that don't offer it.

## Try it

```sh
cargo build --release
./target/release/rein login
./target/release/rein models
./target/release/rein "explain what an agent harness is in two sentences"
cargo test
```

## Known gaps

- The ID token's claims are checked but its signature isn't yet. OpenID
  Connect allows that for tokens fetched straight from the token endpoint
  over TLS, but OpenAI's guide asks for the signature check too.
- One account only, no `rein logout`, and no lock if two processes refresh
  at the same time.

## Next

The agent loop: give the model `read` and `bash`, run what it asks for, and
send the results back until it's done. That's where rein becomes an agent.
