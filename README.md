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

## Models and who pays for them

rein speaks four APIs: OpenAI's Responses API, Anthropic's Messages API,
Gemini's own, and Chat Completions, which most other providers speak. It
knows OpenAI, Anthropic, Google Gemini, OpenRouter, Groq, DeepSeek, xAI,
Mistral, Cerebras, Together, Fireworks, Moonshot and Z.ai, and Ollama and LM
Studio on your own computer; `rein providers` lists them and what pays for
each. A model's name says who serves it (`claude-*` is Anthropic's, `gpt-*`
OpenAI's, `gemini-*` Google's), or you name the provider first:
`anthropic/claude-sonnet-5-5`, `openrouter/qwen/qwen3-coder`,
`ollama/qwen3:8b`. Context windows come from each provider's own model list.

- **Your ChatGPT plan, for OpenAI's models.** Run `rein login` to continue
  with ChatGPT. This uses OpenAI's official [Sign in with ChatGPT](https://developers.openai.com/siwc/token-sharing-open-source)
  for open-source apps. Usage counts toward your plan's limits; on Plus, the
  five-hour limit is shared with every other app that uses your plan. Manage it at
  [ChatGPT settings → Usage](https://chatgpt.com/settings/usage).
- **An API key.** The provider's variable (`OPENAI_API_KEY`,
  `ANTHROPIC_API_KEY`, `GEMINI_API_KEY`, `OPENROUTER_API_KEY`, ...), or a key
  saved with `rein login <provider>`, which checks it first and keeps it in
  `~/.config/rein/keys.json`, readable only by you. The variable wins over the
  saved key. A gateway in front of Claude takes `ANTHROPIC_AUTH_TOKEN`.
  `OPENAI_BASE_URL`, `ANTHROPIC_BASE_URL` and `GOOGLE_GEMINI_BASE_URL` point
  rein at another server that speaks the same API, like a proxy.
- **OpenRouter, signing in in the browser.** `rein login openrouter` uses
  OpenRouter's official [sign-in for apps](https://openrouter.ai/docs/use-cases/oauth-pkce):
  you say yes in the browser, and OpenRouter makes rein a key of its own,
  billed to your OpenRouter credits, which you can see or delete in its
  settings. `--key` pastes one instead.
- **Nothing, on your own computer.** Ollama and LM Studio need no key.

Claude and Gemini plans can't be used: Anthropic and Google don't allow
other apps to sign in with them. rein prints what pays on every run. With no
model named, it uses `gpt-6.1-sol`, or, with no way to pay OpenAI, the first
provider you have a key for (`claude-opus-5-5`, `gemini-3.8-flash`, ...).

Add a provider, or point one of rein's elsewhere, in `~/.config/rein/providers`:

```text
[local]
api = chat                       # responses, anthropic, chat or gemini
url = http://localhost:8080/v1
context = 131072                 # when its model list doesn't say
model = qwen3-coder              # the model to use when none is named

[openrouter]
url = https://proxy.example.com/openrouter/v1
key_env = MY_OPENROUTER_KEY
```

Servers that speak an API don't all take every field of it. When one turns a
request away naming a field rein can do without (`reasoning_effort` on a
server that has none, say), rein sends it again without it and leaves that
field out from then on. What an error can't say, like Mistral's
nine-character tool call IDs, rein knows per provider.

`/model` lists the provider's models and switches to any provider's, in the
middle of a conversation: what was said, and every tool call and result,
carries over in the new provider's format. Reasoning only one provider can
read (OpenAI's encrypted reasoning, Claude's signed thinking, Gemini's
thought signatures) only goes back to that provider. `rein -c` continues with
the model the conversation was left with.

### Claude and Gemini

- **Caching.** Claude caches nothing unless asked, so rein marks the end of
  the system prompt and the end of the conversation in every request; the
  status bar's cache figure says how much each request read from the cache.
- **Claude's thinking.** It comes back signed together with everything before
  it, and Anthropic turns it away if anything earlier in the conversation
  changed. Compaction changes it, so rein then drops the thinking after the
  change, keeps the rest, and records that in the session file. If Anthropic
  still turns some away (from an older rein's tools, say), rein drops it and
  asks again, once.
- **Effort.** `--effort` and `/effort` (`low` to `max`) become Claude's
  effort, Gemini's thinking level, or `reasoning_effort`. Claude models from
  before adaptive thinking get a thinking budget instead.
- **Gemini's signatures.** Gemini's answers carry thought signatures that have
  to come back on the parts they came on, and a function call without one is
  turned away. rein keeps each with its part. A call Gemini didn't make (a
  file you pointed at with `@`, or a call from another provider's turn) is
  marked as such.
- **Refusals.** On Anthropic's own API, Claude models that have a server-side
  fallback ask for it: if a classifier declines by mistake, another model
  answers, and rein says which.

## Usage

```sh
rein login                      # continue with ChatGPT
rein login anthropic            # save an API key, checked first
rein login openrouter           # sign in in the browser; OpenRouter makes rein a key
rein providers                  # who rein can reach, and what pays
rein models                     # list the models you can use, by provider
rein                            # a conversation in this folder
rein "explain what a harness is"   # one task, then exit
rein -c                         # continue the latest conversation here
rein -r                         # choose a saved conversation to continue
rein --model gpt-6.1-sol --effort low "say hi"
rein -m claude-opus-5-5 "explain this repository"
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
| `@` | the project's files and folders that match, as you type, letters in order (`@prsr` finds `src/parser.rs`): ↑↓ pick, Tab puts the path in, Esc closes the list. What you point at goes with your message |
| Shift-Tab | the next mode: ask → edits → read-only |
| Enter while rein works | steer it: your message goes into the turn at its next step, and steps rein had planned but not started are skipped. Commands wait for the end of the turn |
| Tab while rein works | send your message after the turn instead. Esc puts what's waiting back in the box |
| Esc or Ctrl-C while rein works | stop that turn; the conversation goes on |
| Ctrl-C at the prompt | clear the line; press again within two seconds to leave |
| Alt-← / Alt-→ | a word left or right |
| Ctrl-O | see every step's full output: ↑↓ scroll, ←→ other steps, `/` search, Esc back |
| Mouse wheel, PgUp / PgDn, Shift-↑ / ↓ | scroll the conversation |
| Drag over text | select it and copy it to the clipboard |
| Ctrl-L | draw the window again |
| `/mode [ask\|edits\|read-only\|trust]` | show or change what runs without asking (also `/permissions`) |
| `/model [id]`, `/effort [level]` | list or switch the model, from any provider; show or change reasoning effort |
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
With Claude, its thinking from after the point compaction changed goes with
it (see above); the words stay.

## Settings

Defaults go in `~/.config/rein/config`, one per line:

```text
model = "gpt-6.1-sol"
effort = "medium"
mode = "edits"
compact_at = 120000
bell = off
compress = off
```

A flag wins over `$REIN_MODEL`, `$REIN_EFFORT`, `$REIN_MODE`,
`$REIN_COMPACT_AT`, `$REIN_BELL` and `$REIN_COMPRESS`, which win over the
file.

## Small things that matter

- A request that fails before anything streams (network trouble, a busy
  server, a rate limit) is retried, 1, 2 and 4 seconds apart.
- Commands are stopped after 60 seconds; the model can ask for up to 10
  minutes for a slow build or test suite.
- For work with several steps the model keeps a plan, shown as a checklist.
- What the model thought about shows as one dim line: `✻ Checking the tests`.

## What goes to the model

Nothing is uploaded up front: no files, no folder listing. The model asks
for what it needs with `read`, `grep` and `glob`, and gets it back as text,
as part of the conversation, or you point at it with `@` in your message:

```text
why does @src/parser.rs panic?      the file, read with your message
look at @src/parser.rs:120-180      those lines (or #L120-L180)
what's in @src/ui/?                 the folder's files
```

What you point at is read for you, as a `read` step under your message, so
the model starts with it instead of spending a round trip on it. A file the
conversation already holds as it is now goes as a one-line note instead of a
second copy, and a file in a folder with its own AGENTS.md brings that along,
once. `@alice` is just a word unless there's a file called that; a path that
isn't there says so. Reads the model asks for together run at once, and a
run of reads shows as one line, names and all: `● Read  a.rs, b.rs +2  4
files · 612 lines`. Each request carries the whole conversation,
since the server keeps none of it, so a long one sends the same hundreds of
kilobytes again and again.

So rein gzips every request body. `api.openai.com` unpacks it before the
model sees anything, so the model reads the same text and the tokens are the
same; what changes is the upload, about three times smaller for rein's
JSON, which shows on a slow connection. `/status` says how much went and how
much it saved. A server that turns a compressed request away, as many local
ones behind `$OPENAI_BASE_URL` would, gets the same request again
uncompressed, and nothing compressed after that; rein says so once.
`compress = off` in the settings turns it off.

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

rein's tools are `read`, `grep`, `glob`, `write`, `edit` and `bash`. They
only touch files inside the folder you run it in. Before a file change you
see the lines it removes and adds.

`grep` searches what files say, with a regular expression, and `glob` finds
files by name or path, like `src/**/*.rs`. Both only look, so they never
ask, in any mode, and they're how the model finds its way around instead of
`grep` or `rg` in `bash`. They skip what the project's `.gitignore` files
leave out, binary files, `.git` and anything behind a symlink, and they
search on every core at once. At each question, answer `y`, `n`, `a` for "yes, and
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
| `crates/rein-llm` | Streaming LLM provider layer: one client for every provider, the Responses, Messages, Gemini and Chat Completions APIs, the providers rein knows, Sign in with ChatGPT |
| `crates/rein-agent` | The agent loop, the tools, the permission engine, saved conversations, and what the model is told first |
| `crates/rein-cli` | The `rein` command |

## Tests

```sh
cargo test
```

The tests replay saved response streams from `crates/rein-llm/tests/fixtures`,
so they need no network and spend no tokens. `crates/rein-cli/tests/screen.rs`
runs the real `rein` in a pseudo-terminal, against a model that follows a
script, speaking each API rein does, and reads the screen through a terminal
emulator kept in memory: it
types, scrolls, approves commands, resizes the window, leaves and comes back,
and checks what you would see, and when the bell rang and what was copied.
It also checks what goes over the wire: that each block of Claude's thinking
rein sends back follows exactly the conversation it was signed with, through
compactions, as Anthropic checks; that Gemini's signatures go back on their
parts; and that a field a server turned away isn't sent to it again.
`crates/rein-cli/tests/e2e.rs` runs rein against real providers, with your
keys: an answer with a tool call from each one rein can pay (or that's
running, for Ollama), Claude's cache across two requests, a conversation
moved from one provider to another, and Claude through a compaction. It's
off unless asked, since it costs a little:

```sh
REIN_E2E=1 cargo test --test e2e -- --nocapture --test-threads 1
```

Record a real stream with
`rein --record crates/rein-llm/tests/fixtures/<name>.sse "<prompt>"`. Recordings
contain your account's `safety_identifier`; change it to `user-REDACTED` before
committing. A test fails if you forget.
