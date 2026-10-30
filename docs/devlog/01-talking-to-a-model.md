# 01: Talking to a model

**Code:** `git checkout ep01`

rein can sign in with your ChatGPT plan and stream an answer:

```
$ rein "Say exactly: Hello, world!"
Using ChatGPT plan (manage usage: https://chatgpt.com/settings/usage)
Hello, world!
[13 in (0 cached), 8 out (0 reasoning)]
```

That's not an agent yet. It's the layer every agent sits on.

## The idea: the model remembers nothing

A model is a stateless function: text in, text out. It has no memory, can't
touch your files, and can't run anything. A harness supplies the rest. It
keeps the conversation and **resends all of it on every request**, runs
tools, decides what's safe, and handles credentials.

You can see this in the request rein sends: `store: false`, and `input`
holds the whole conversation. OpenAI keeps nothing between calls, so rein is
the memory.

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
| SSE parser | `crates/rein-llm/src/sse.rs` | Answers stream in as small events. The network splits them anywhere, even inside an emoji, so the parser buffers bytes until it has whole lines. |
| rein's own events | `Event` in `lib.rs` | OpenAI sends dozens of event types. rein turns them into a few of its own (`TextDelta`, `Completed`), so the agent loop never depends on one provider's format. Adding Anthropic later means writing one more decoder. |
| Exact endings | `EventDecoder` in `openai.rs` | A turn only counts as done on `response.completed`. A dropped connection is `Interrupted`, never "done", so the agent can't act on half an answer. |
| Sign in with ChatGPT | `openai/signin.rs` | OAuth with PKCE, a tiny local server for the browser callback, and token refresh. Refresh tokens rotate, so new ones are saved at once. |
| Private storage | `store.rs` | Credentials are written atomically, readable only by you (`0600`). |
| Replay tests | `tests/replay.rs` | The model is random; the harness shouldn't be. We record real answers once and test against them forever: no network, no tokens. |

## Decisions and plot twists

- **Rust.** Hello-world binaries measured on a Mac: Bun 55 MB, Go 5.4 MB,
  Rust 279 KB. rein's release binary, including TLS, HTTP, JSON and the
  whole sign-in flow, is **2.2 MB** and starts in about **4 ms**.
- **Not Claude, for now.** Anthropic's terms forbid using Claude Pro/Max
  subscriptions in third-party harnesses (February 2026), and accounts get
  suspended for it. API keys are fine, but cost money per token.
- **Not the Codex login either.** The first plan was to reuse the Codex CLI's
  saved login against `chatgpt.com/backend-api`. Then it turned out OpenAI had
  launched an official
  [Sign in with ChatGPT for open-source agents](https://developers.openai.com/siwc/token-sharing-open-source)
  (opened around 2026-09-29), and its docs say not to use the backend
  endpoints. rein uses the official flow and the public
  `api.openai.com/v1/responses`. The same code works with an API key.
- **The recording that knew who I was.** Recorded streams include a
  `safety_identifier`: your OpenAI user ID. Fixtures are public, so it's
  redacted, and a test fails if a real one ever gets committed.

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
