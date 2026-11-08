# The rein guide

Everything rein does, and how to use it. The [README](../README.md) is the
short version.

**Contents**

1. [Install](#install)
2. [Models and signing in](#models-and-signing-in)
3. [A conversation](#a-conversation)
4. [Keys and commands](#keys-and-commands)
5. [What the model sees](#what-the-model-sees)
6. [Long conversations](#long-conversations)
7. [How a turn runs](#how-a-turn-runs)
8. [Background commands](#background-commands)
9. [Checks after edits](#checks-after-edits)
10. [Web search](#web-search)
11. [Images](#images)
12. [What rein may do](#what-rein-may-do): tools, modes, rules, the sandbox, undo
13. [Skills](#skills)
14. [Hooks](#hooks)
15. [Memory](#memory)
16. [MCP servers](#mcp-servers)
17. [Reins in other tabs](#reins-in-other-tabs), and [sub-agents](#sub-agents)
18. [What's left of your limits](#whats-left-of-your-limits)
19. [Settings](#settings)
20. [For contributors](#for-contributors): layout and tests

---

## Install

On macOS or Linux, on ARM or Intel:

```sh
curl -fsSL https://github.com/FurkanCodes/rein/releases/latest/download/install.sh | sh
```

[`install.sh`](../install.sh) picks the build for your computer from the
release, checks it against the release's checksums, makes sure it runs, and
only then puts `rein` in `~/.local/bin`. It needs no sudo, and `rein
--version` says what you have. Run it again to update. `REIN_VERSION=0.1.0`
installs that version, and `REIN_INSTALL_DIR` another folder.

If that folder isn't on your `PATH`, it offers to add it in your shell's
startup file (zsh, bash or fish), and changes nothing unless you say yes.
With another shell, or with nobody at the terminal to answer, it shows the
line to add instead. It shows a progress bar and colour only on a terminal,
and `NO_COLOR` turns the colour off.

[Each release](https://github.com/FurkanCodes/rein/releases/latest) has the
builds to download by hand too. They aren't signed, so macOS stops one
downloaded in a browser until you run `xattr -d com.apple.quarantine rein`.
The Linux builds need glibc 2.34 or newer (Ubuntu 22.04 or later), and
there's none for musl (Alpine). There's no build for Windows itself, where
rein doesn't compile yet: run the Linux one in WSL.

To build it yourself, with Rust 1.95 or newer (1.92 can't build rein), from
a clone of this repo:

```sh
cargo install --path crates/rein-cli --locked   # puts `rein` on your PATH
cargo build --release && ./target/release/rein --help   # or just build it
```

## Models and signing in

rein speaks four APIs: OpenAI's Responses API, Anthropic's Messages API,
Gemini's own, and Chat Completions, which most other providers use. It knows
OpenAI, Anthropic, Google Gemini, OpenRouter, Groq, DeepSeek, xAI, Mistral,
Cerebras, Together, Fireworks, Moonshot and Z.ai, plus Ollama and LM Studio
on your own computer. `rein providers` lists them and which you're signed in to.

A model's name says who serves it (`claude-*` is Anthropic's, `gpt-*`
OpenAI's, `gemini-*` Google's), or you name the provider first:
`anthropic/claude-sonnet-5-5`, `openrouter/qwen/qwen3-coder`,
`ollama/qwen3:8b`. Context windows come from each provider's own model list.

| What you use | How to set it up | Good to know |
|---|---|---|
| **Your ChatGPT plan** | `rein login` | OpenAI's official [Sign in with ChatGPT](https://developers.openai.com/siwc/token-sharing-open-source) for open-source apps. It counts toward your plan's limits. On Plus, the five-hour limit is shared with every app that uses your plan ([ChatGPT settings → Usage](https://chatgpt.com/settings/usage)). |
| **An API key** | The provider's variable (`OPENAI_API_KEY`, `ANTHROPIC_API_KEY`, `GEMINI_API_KEY`, `OPENROUTER_API_KEY`, …), or `rein login <provider>`, which asks for it, or `rein login <provider> <key>` (your shell keeps that line) | Checked before it's saved, never shown again, and kept in `~/.config/rein/keys.json`, readable only by you. The variable wins over the saved key. |
| **OpenRouter, in the browser** | `rein login openrouter` | OpenRouter's official [sign-in for apps](https://openrouter.ai/docs/use-cases/oauth-pkce). You approve in the browser, and OpenRouter makes a key for rein, billed to your credits, which you can see or delete in OpenRouter's settings. `--key` pastes a key instead. |
| **Nothing** | Ollama, LM Studio | No key needed. |
| **Your Claude plan** | the `claude-code` provider | Runs Anthropic's own CLI. See [below](#a-claude-plan-through-claude-codes-cli). |

You don't have to set any of this up first. The first time you run `rein`,
or whenever you're signed in to nothing that runs the model, it opens on
the same menu `/login` shows. Pick a provider and sign in, or paste its key
into the box (it shows as dots), and the conversation starts. For a
provider rein has no starting model for, such as Groq, rein then lists its
models. The one you pick is saved as `model` in the [settings](#settings),
so the next `rein` starts with it. Press Esc to leave. A one-off task
(`rein "…"`) has no one to ask, so it tells you how to sign in instead.

A sign-in in the browser opens the one in `$BROWSER` if that's set, or else
your system's default. In WSL that's your Windows browser, which comes back
to rein through WSL's forwarding of `localhost`. If no browser opens, rein
shows the link: drag over it to copy it.

rein doesn't sign in to Claude or Gemini plans itself, because Anthropic and
Google don't allow other apps to. Every run shows which account it uses.
With no model named, it uses `gpt-6.1-sol`. If you're not signed in to
OpenAI, it uses the first provider you have a key for (`claude-opus-5-5`, `gemini-3.8-flash`,
…).

A gateway in front of Claude takes `ANTHROPIC_AUTH_TOKEN`.
`OPENAI_BASE_URL`, `ANTHROPIC_BASE_URL` and `GOOGLE_GEMINI_BASE_URL` point
rein at another server that speaks the same API, such as a proxy.

### Your own providers

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

Servers that speak an API don't all take every field of it. When one
rejects a request and names a field rein can do without (`stream_options`
on a server that has none, say), rein sends it again without that field and
leaves it out from then on. An effort level is left out only for the model
that rejected it. rein knows per provider what an error can't say, like
Mistral's nine-character tool call IDs.

### Switching models

`/model` lists the provider's models and switches to any provider's model
mid-conversation. The messages, and every tool call and result, carry over
in the new provider's format. Reasoning that only one provider can read
(OpenAI's encrypted reasoning, Claude's signed thinking, Gemini's thought
signatures) goes back only to that provider. `rein -c` continues with the
model the conversation was last using.

### A Claude plan through Claude Code's CLI

Install the official `claude` CLI, then either:

- run `claude auth login`, then `rein login claude-code`, and press Enter to
  use the CLI's stored login; or
- run `claude setup-token`, then `rein login claude-code`, and paste the
  token at the hidden prompt. This suits unattended use. Renew the token
  through the CLI when it expires.

Then use `rein -m claude-code/sonnet`, `claude-code/opus`, `claude-code/haiku`
or `claude-code/<full-model-id>`. Inside rein, `/login claude-code` takes a
token (or an empty answer for the CLI's login), and `/model
claude-code/sonnet` switches. `CLAUDE_CODE_OAUTH_TOKEN` works instead of a
saved token. For a service, put `claude` on its `PATH`, since shell profiles
aren't loaded.

How it runs:

- rein runs `claude -p`, never sends the token through its own HTTP
  provider, and strips inherited API keys, endpoint overrides and
  alternate-provider switches from the CLI's environment.
- The token reaches the CLI through a pipe instead of its environment, where
  other programs could read it.
- Each run happens in its own empty private folder, with
  `--setting-sources user`. Otherwise a project's `.claude/settings.json`
  could send your token to another server or run a command of its choosing.
  rein doesn't use `--bare`. It needs a recent CLI, with `--safe-mode` and
  `--json-schema`.
- The CLI's customizations, MCP servers and built-in tools are off.
  Structured answers ask for rein's tools, which still go through rein's
  approvals and sandbox.
- Each step starts a fresh CLI with the conversation, so text appears when
  the step is done, not word by word. Output is capped at 16 MiB per pipe.
  A quiet CLI times out after five minutes, and a step after ten.

rein keeps saved tokens in a private `claude-code.json` in its folder.
`rein logout claude-code` removes it, but doesn't sign the CLI out, revoke a
token, or unset an environment token. Anthropic's terms set what a plan
allows, its limits and a token's lifetime.

An earlier rein offered `rein login anthropic --oauth`, which signed in to a
Claude plan *as* Claude Code, with its client ID, headers and system prompt.
It was removed because rein never signs in as another app. If
`claude-oauth.json` is still in rein's folder, rein deletes it and says so.

### Provider details

- **Claude's cache.** Claude caches nothing unless asked, so rein marks the
  end of the system prompt and the end of the conversation in every request.
  The status bar's cache figure shows how much each request read from the
  cache.
- **Claude's thinking.** Thinking comes back signed together with everything
  before it, and Anthropic rejects it if anything earlier has changed.
  Compaction changes the conversation, so rein drops the thinking after the
  change, keeps the rest, and records that in the session file. If Anthropic
  still rejects some (from an older rein's tools, say), rein drops it and
  retries once.
- **Effort, per model.** Each model takes its own effort levels, and most
  providers' model lists say which. Sign in with ChatGPT, Anthropic,
  OpenRouter, xAI and DeepSeek list them, and Sign in with ChatGPT and
  OpenRouter name each model's default. Gemini's levels go by
  generation. `/effort` offers only the levels the current model takes, and names its
  default. You choose a level once (`off`, `minimal`, `low`, `medium`,
  `high`, `xhigh` or `max`), and each model gets the closest it has without
  going over: `max` on a model whose top is `xhigh` sends `xhigh`. The bar
  shows what was sent. Where the list says nothing (an OpenAI API key, Groq,
  Ollama), every level is offered, and one the model turns down is reported
  once and not sent again. Claude models from before adaptive thinking get a
  thinking budget.
- **Gemini's signatures.** Gemini's thought signatures must come back on the
  parts they arrived on, and Gemini rejects a function call without one.
  rein keeps each signature with its part, and marks the calls Gemini didn't
  make (a file you pointed at with `@`, another provider's turn).
- **Refusals.** On Anthropic's own API, Claude models with a server-side
  fallback ask for it. If a classifier declines by mistake, another model
  answers, and rein says which.

## A conversation

```sh
rein login                      # continue with ChatGPT
rein login anthropic            # save an API key, checked first
rein login anthropic sk-ant-…   # the same, with the key given here
rein login openrouter           # sign in in the browser; OpenRouter makes rein a key
rein providers                  # the providers, and which you're signed in to
rein models                     # the models you can use, by provider
rein                            # a conversation in this folder
rein "explain what a harness is"    # one task, then exit
rein -c                         # continue the latest conversation here
rein -r                         # choose a saved conversation to continue
rein --model gpt-6.1-sol --effort low "say hi"
```

**The screen.** rein takes the whole window, as `less` and `vim` do, with
the conversation above a message box at the bottom. The status bar under
the box shows the model and its effort, the mode, how full the context is,
how much of your limits is used (where the provider says), how much came
from the cache, and how long the last turn took. While rein works, a line
above the box says what it's doing and for how long.

**Talk and work.** What you and the model say stands apart from what it
does. Your messages sit under `you` on a band across the window. rein asks
the terminal for its background colour and tints the band from it, so it
suits a dark theme or a light one; where the terminal doesn't say, your
message is bold instead. The model's words start with a white `●` under
`rein`, and answers are rendered from Markdown.

Its work is quieter. Each step is a grey line, two columns in from the
model's words: ✓ when it worked, ✗ when it didn't, ○ when it didn't run
(declined or skipped), then the tool (`Read`, `Edit`, `Run`) and a short
result, like a command's exit code and the last line it printed. A
command that failed shows the lines that say why under it. Reads and
searches in a row fold into one line (`Read 2 files, searched for 1
pattern`). An edit keeps its name bold and says how many lines it adds
and takes away (`+5 -1`). The lines themselves show on green and red tints
under it, before you approve it when rein asks. Ctrl-O shows every step's
whole output. A turn that ran steps ends with what it came to:
`── 4 steps · changed slug.py · 24s ──`.

**The window.** rein keeps the conversation as text and lays it out for the
window's width, so resizing loses nothing and words wrap where they should.
Scroll with the mouse wheel, PgUp/PgDn or Shift-↑↓, and drag over text to
copy it: a line wrapped over rows comes out as one line, and a link broken
over rows as one link. What the terminal showed before rein is left as it was. When you
leave, rein prints the conversation there, so it stays in the scrollback.
`rein "task"` and output that isn't a terminal get plain lines instead.
`NO_COLOR` turns colour off.

**Questions.** When rein needs your go-ahead, the box turns yellow and asks,
with the whole command in view. **y** is yes, **a** is yes and don't ask
again, **n** is no, and **Esc** is no and stop, so you can say what to do
instead. The arrows and Enter, or 1 to 4, work too. Keys typed the moment a
question appears still go to your message, so typing ahead can't answer it
by accident.

**Saved as it goes.** Each message, tool call and result is written as one
JSON line to `~/.config/rein/sessions/<folder>/`, which only you can read.
`rein -c` picks up the latest conversation in the folder you're in, with the
same history, system prompt and cache key, so it usually starts with a warm
cache. Your messages are also kept for ↑ and Ctrl-R in
`~/.config/rein/history/`: the last 1,000 from each folder, without
duplicates.

**The bell.** rein rings the terminal's bell when it needs you and you're
away, because it's asking something or it's done. Away means its window
isn't in front, which most terminals report
(tmux does with `set -g focus-events
on`). Where the terminal doesn't say, it means you haven't touched a key for
10 seconds. `bell = off` turns it off.

## Keys and commands

| Key | What it does |
|---|---|
| ↑ / ↓ | messages you sent before from this folder, in this run or an earlier one |
| Ctrl-R | search what you sent before by its words: ↑↓ or Ctrl-R picks, Enter puts it in the box, Esc goes back |
| Shift-Enter, Ctrl-J, or `\` at a line's end | a new line in your message |
| `/` | the commands and skills that match what you type: ↑↓ picks, Tab completes, Enter runs |
| `@` | the project's files and folders that match the letters you type, in order (`@prsr` finds `src/parser.rs`). Tab puts the path in, Esc closes the list |
| Ctrl-V | paste an image: `[Image #1]` in the box ([Images](#images)) |
| Shift-Tab | the next mode: trust → ask → edits → auto → read-only |
| Enter while rein works | steer it. Your message goes in at its next step, and planned steps it hasn't started are skipped |
| Tab while rein works | send your message after the turn instead; Esc puts it back in the box |
| Esc or Ctrl-C while rein works | stop the turn; the conversation goes on |
| Ctrl-C at the prompt | clear the line; press again within two seconds to leave |
| Alt-← / Alt-→ | a word left or right |
| Ctrl-O | the full output of every step and background command: ↑↓ scroll, ←→ other steps, `/` search, Esc back |
| Ctrl-B while a command runs, nothing typed | the command goes on in the background, and so does the turn; with something typed, it moves the cursor a letter left |
| Mouse wheel, PgUp / PgDn, Shift-↑↓ | scroll the conversation |
| Drag over text | select and copy it |
| Ctrl-L | redraw the window |
| Ctrl-D or `/exit` | leave |

| Command | What it does |
|---|---|
| `/model [id]`, `/effort [level]` | switch to a model from any provider; change the effort to a level the model takes |
| `/mode [trust\|ask\|edits\|auto\|read-only]` | what runs without asking, which you can keep for new conversations ([Modes](#modes)) |
| `/sandbox [on\|off]` | run commands in the sandbox, or as you'd run them, in any mode ([The sandbox](#the-sandbox)) |
| `/permissions [add\|remove <rule>]` | what asks first or is refused ([Rules](#rules)) |
| `/undo` | put back what rein did to the project's files since a turn you pick ([Undo](#undo)) |
| `/changes` | what this conversation changed in the project's files, file by file ([Undo](#undo)) |
| `/check [on\|off\|command]` | the project's check after edits ([Checks](#checks-after-edits)) |
| `/tasks` | the commands running in the background, to stop one |
| `/search [auto\|off\|model]` | which model searches the web ([Web search](#web-search)) |
| `/skills`, `/<skill> [args]`, `/skill:<name>` | your skills: put one in the box, turn one off or on, or call one with your message ([Skills](#skills)) |
| `/hooks` | your hooks and how each last ran, to turn one off or on ([Hooks](#hooks)) |
| `/memory` | the notes kept across conversations, to read or forget one ([Memory](#memory)) |
| `/mcp`, `/mcp add`, `/mcp import`, `/mcp allow`, `/mcp remove` | the MCP servers: add, import, change or remove ([MCP](#mcp-servers)) |
| `/send`, `/delegate`, `/peers`, `/inbox`, `/spawn` | other reins and sub-agents ([Reins in other tabs](#reins-in-other-tabs)) |
| `/name`, `/workspace` | how the other reins know this one |
| `/login [provider] [key]`, `/logout [provider]` | sign in or out without leaving. A key typed in the box shows as dots. `/login openrouter <key>` takes the key as given, and the line isn't shown again or kept for ↑. OpenRouter and `chatgpt` sign in through the browser |
| `/new` (or `/clear`), `/resume`, `/status` | a fresh conversation; continue a saved one; what's in use and how much of your limits is used |
| `/compact [focus]` | make room now; the summary keeps `focus` in mind |
| `/copy` | copy rein's last answer as written, Markdown included |
| `/help` | all of them |

**Menus.** A command given no choice shows what you can choose from:
`/model` lists every provider's models, `/login` the providers, and so on.
↑↓ moves (PgUp/PgDn a page), Enter picks, Esc changes nothing, and the
one in use is marked •. Typing narrows the list. Each word you type must
be in an item's name or description, and names rank first. In a name, a
word's letters also count when each starts a word or follows the one
before, so `vrbp` finds `vercel-react-best-practices`. Scattered letters
never count in a description, where almost any word's letters turn up
somewhere: `react` kept 171 of 211 skills that way (for
"pRactices"), and now keeps 16. Matches are marked, a description that
matched past the edge is shown from the match, and the box says how many
match: `16 of 211`.

**Pastes.** A big paste shows in the box as one piece, `[pasted 40 lines]`,
which Backspace deletes whole. The model still gets all of it.
## What the model sees

Nothing is sent up front: no files, no folder listing. The model asks for
what it needs with `read`, `grep` and `glob`, or you point at it with `@` in
your message:

```text
why does @src/parser.rs panic?      the file, read with your message
look at @src/parser.rs:120-180      those lines (or #L120-L180)
what's in @src/ui/?                 the folder's files
```

What you point at is read for you, as a `read` step under your message, so
the model starts with it instead of spending a round trip. A file the
conversation already holds as it is now goes as a one-line note instead of
a second copy. A file in a folder with its own AGENTS.md brings that file
along, once. `@alice` is just a word unless a file has that name, and rein
reports a missing path.
Reads requested together run at once, and consecutive reads show as one
line: `✓ Read   a.rs, b.rs +2  4 files · 612 lines`.

**What rein tells the model.**

- **A short system prompt** of its own (`crates/rein-agent/src/prompt.rs`),
  with nothing in it that changes over time. `-s "..."` adds your standing
  instructions.
- **A first message** with the working folder, the operating system, today's
  date (no time of day) and the project's instructions: the `AGENTS.md`
  files from the repository root down to the folder you're in (`CLAUDE.md`
  where a folder has none), plus your own `~/.config/rein/AGENTS.md`, 32 KB
  at most. It's saved with the conversation, so a continued one starts with
  the same text.

**Smaller uploads.** Each request carries the whole conversation, since the
server keeps none of it, so rein gzips every request body.
`api.openai.com` unpacks it before the model sees anything, so the tokens
are the same, but the upload is about three times smaller, which shows on a
slow connection. `/status` says how much it saved. If a server turns a
compressed request away, rein sends it again uncompressed, compresses
nothing after that, and says so once. `compress = off` turns it off.

## Long conversations

The status bar shows how full the context is: `context 42k/150k`. Past the
limit (150,000 tokens, or `compact_at`, never more than 90% of the model's
window), rein makes room, cheapest first:

1. **Stale tool output is retired:** an old read of a file read or changed
   again later, and command output from before your last two messages. Each
   becomes one line saying what was there. If that's enough, rein stops
   there.
2. **Otherwise the model writes a summary.** It's asked with the
   conversation as it was plus one more message, so the request is mostly
   served from the cache. rein adds its own record of your messages, the
   files changed and the commands run, so those facts can't be
   misremembered. The conversation goes on from the first message, the
   summary, and its latest part word for word.

Nothing is lost: the session file keeps everything, and the model's
`recall` tool searches the whole conversation, compacted parts included. A
long turn is compacted as often as it needs, but never twice without a step
in between, and not again after a compaction that made no room. With
Claude, thinking from after the point compaction changed is dropped; the
text stays.

## How a turn runs

- **Until it's done.** A turn goes on until the model is done, however many
  steps that takes, as in Claude Code, Codex and pi. You're watching, and
  Esc stops it.
- **Loops.** When the same call (or a short cycle of calls) gives the same
  result three times in a row, rein tells the model, and at five it stops
  the turn and says so. A call repeated while its result changes, like a
  build getting further, is progress. Type `continue` to go on.
- **A turn nobody watches** (`auto` mode, or `rein "task"`) stops after
  `max_steps` steps, 200 by default (`--max-steps`, or `off`). Its last step
  tells the model, which says what it did and what's left, so you come back
  to a report. A task cut off like this exits with an error.
- **Retries.** A request that fails before anything streams (a network
  error, a busy server, a rate limit) is retried 1, 2 and 4 seconds apart.
- **A plan.** For work with several steps, the model keeps a checklist.
- **Thinking** shows as one dim line: `✻ Checking the tests`.
- **Command output** comes back as its start and a longer part of its end,
  where it says how it went.

## Background commands

The model can run a dev server, a watcher or a long test suite in the
background (`bash` with `background`) and keep working. rein answers at once
with its name, `b1`, and keeps what it prints: the first 64 KB and the
latest 1 MB. With its `background` tool the model reads what's new, waits
for a line (`until: "ready"`, up to 10 minutes), or stops it.

The model never has to poll: when one ends, it's told once, with its exit
code and last lines, in the current turn if one is running, or in a turn of
its own if you're not typing. If you stop one yourself, the model hears with
your next message.

- **What goes to the background.** A command still running at its timeout
  (60 seconds, or up to 10 minutes when the model asks), one you press
  Ctrl-B on, and one that ends but leaves something running (`npm run dev
  &`). Otherwise that server holds the output open, and the turn waits until
  the timeout kills it.
- **What you see.** A line under the box for each (`▸ b1 npm run dev · 2m 03s
  · ready on :3000`); Ctrl-O shows what each printed; `/tasks` stops one.
- **Limits.** At most 8 run at once. A ninth is refused with the list, so
  the model stops one itself and nothing is dropped behind its back.
- **Walls and undo.** They keep the walls they started in, and `/undo`
  counts what they change in the turn it's seen in.
- **Ending.** They stop with the conversation (leaving, `/new`, `/resume`,
  the end of `rein "task"`) and with the tab. Each is asked to stop, then its
  whole process group is killed 2 seconds later. A compaction lists what's
  still running, so the model doesn't start a second server.

## Checks after edits

After a step changes the project's files, with an edit or a command (`sed`,
a formatter), rein runs the project's own check and tells the model at once
what its changes broke, not three steps later when a build fails.

| The project has | The check |
|---|---|
| `Cargo.toml` | `cargo check --workspace --all-targets` |
| `tsconfig.json`, and its own `tsc` | `tsc --noEmit` |
| `go.mod` | `go vet ./...` |
| pyright or mypy set up | `pyright` or `mypy` |

- **Asked once per project.** The check runs the project's own code (build
  scripts), in the sandbox when that's on. Your answer is kept in rein's
  folder by the exact command, so a different one asks again.
- **Only what the turn broke.** Errors from before its first change are
  left out. They're compared by file, code and message, never by line, so an
  old error an edit moved isn't new. Only errors are sent, never warnings:
  the first 10, each with its path from the project root:

  ```text
  [rein checked the project with `cargo check` after these changes: 1 new error.
  src/lib.rs:2:5 error[E0425]: cannot find value `BROKEN` in this scope]
  ```
- **When nothing broke.** If a check finds no new errors, nothing goes into
  the conversation. Once the errors are fixed, rein says so, once. If the
  model says it's done while errors it introduced remain, it's told, once.
- **Under the step:** `↳ cargo check · no new errors · 0.4s`, or `✗ cargo
  check · 1 new error` with the first few. Ctrl-O shows them all.
- **Speed.** On rein itself, `cargo check` takes 0.3 to 0.7 s after an edit.
  A check over 15 s is stopped, and checking pauses for this conversation.
  One waiting for a build another command holds is skipped for that step.
- **Your own.** `/check` shows what runs, `/check off` and `/check on` switch
  it for the project, and `/check <command>` runs yours. `check = off` turns
  it off everywhere, and `check = <command>` runs yours everywhere.

Unlike a language server, the project's command says what the build says
and needs nothing installed. In other harnesses, language servers' errors
came a step late, went stale after a `sed`, came back as new, or the
servers hung or were left running. OpenCode's own docs now advise running
the type check instead.

## Web search

The model searches the web with `web_search` and decides when. Its
description says it's for what changes or what the model may not know (a
library's current version, its docs, an error message, anything after its
training), never for what's in the project. The model asks a question in
words, and a separate request to the model chosen for searching runs that
provider's own search, reads what it needs, and answers with its sources.

```
✓ Search  the latest stable Rust release   1 source: blog.rust-lang.org · gpt-6.1-sol, cached
  └ Rust 1.99.1, released 2026-10-15…
```

Only the answer goes into the conversation, not the pages. On a ChatGPT
plan, a search added about 780 tokens. OpenAI's search inside the
conversation added 12,000, sent again with every later request.

- **Who searches.** The model in use, if its provider can (OpenAI's models,
  on a ChatGPT plan too, Anthropic's and Claude Code's), or else the first
  of those you're signed in to, so a Gemini or local model searches too.
  `/search` picks another, or `off`. Gemini's own search isn't used:
  Google's terms require showing its suggestions, which a terminal can't.
- **How far it reaches** depends on the mode. In `trust`, where a command
  can reach any site anyway, search is live. Elsewhere it uses OpenAI's
  index and opens no page as it is now, nor one at an address the model made
  up, so a page can't get the model to send the project out in a link.
  Anthropic's and Claude Code's searches only search.
- **What a search finds** is marked as the web's, not yours, and the
  searching model is told not to do what a page says.

## Images

The model sees images that tools give it: a screenshot from an MCP server
(Chrome DevTools, Playwright), a `.png`, `.jpg`, `.gif` or `.webp` that
`read` opens, one you name with `@`, or one you paste with Ctrl-V.

```
✓ Read   colors.png   image, 240×120
```

- **Each goes as its provider takes it:** inside the tool's result for
  OpenAI (a ChatGPT plan too), Anthropic and Gemini; in a message after the
  results for Chat Completions, whose tool messages take only text; and to
  `claude -p` as streamed JSON.
- **A model that can't see** (its list says so, or it turned one down,
  learned per model) is told of the image in words, and the image stays for
  one that can.
- **Shrunk when too big.** A PNG over 2,000 pixels a side or 5 MB is shrunk,
  and the model is told by how much, so it can find a point in the
  original. A JPEG, GIF or WebP that fits goes as it is.
- **Never sent broken.** rein says when an image can't be shown and doesn't
  send it: an image a provider turns away would fail every request after it.
- **Stored once,** as a file named by its contents, beside the project's
  conversations, which name it and hold none of its bytes. At compaction, an
  image from before your last two messages makes room, and its result says
  how to see it again.
- **Pasting.** On macOS, Ctrl-V reads the clipboard's PNG, or its TIFF (a
  copied screenshot) as a PNG; on Linux, `wl-paste` or `xclip`. Over SSH,
  put the image in the project and name it with `@`.

## What rein may do

rein's own tools are `read`, `grep`, `glob`, `write`, `edit` and `bash`, and
they touch only files inside the folder you run it in. Before a file
changes, you see the lines removed and added.

`grep` searches file contents with a regular expression, and `glob` finds
files by name or path, like `src/**/*.rs`. Both only look, so they never
ask, in any mode. The model uses them to find its way around instead of
`grep` or `rg` in `bash`. They skip what `.gitignore` leaves out, binary
files, `.git` and anything behind a symlink, and they search on every core
at once.

What runs without asking depends on the **mode**, your **rules** and, if you
turn it on, the **sandbox**.

### Modes

rein starts in `trust`, where everything runs without asking except what
your rules say to ask about first, which at the start is deleting.
`--mode`, `mode` in the settings, `/mode` (which offers to keep what you
pick) or Shift-Tab change it.

`trust` is dangerous, and rein marks it with ⚠ wherever it names it: a
command can do anything you can, so a bad instruction in a file or on a web
page the model reads could cost you your files or your keys. `auto` gets the
same mark with the sandbox off.

With the sandbox **off** (the default), nothing holds a command to what it
says it needs:

| Mode | File changes | Commands |
|---|---|---|
| `trust` (default) | allowed | run |
| `ask` | ask | ask |
| `edits` | allowed | ask |
| `auto` | allowed | run |
| `read-only` | refused | ask, if they only look; the rest are refused |

With the sandbox **on**, the mode goes by what a command needs:

| Mode | File changes | `look` | `write` | `network`, `outside` |
|---|---|---|---|---|
| `trust` | allowed | runs | runs | runs |
| `ask` | ask | runs | ask | ask |
| `edits` | allowed | runs | runs | ask |
| `auto` | allowed | runs | runs | refused |
| `read-only` | refused | runs | refused | refused |

- **`trust`** runs every command without asking, in the sandbox when it's on
  (or outside it, for a command that needs `outside`), except that ask rules
  still ask and deny rules still block. What the model reads (a web page, an
  issue, a file in a cloned repository, an MCP server's answer) can lead it
  to run any command with your keys and the network. Rules are for
  mistakes, not for that. For work you'd rather not watch, turn the sandbox
  on: its walls hold whatever the model is told.
- **`auto`** never asks. It's for leaving rein to work on its own, so
  whatever would ask is refused instead. With the sandbox on, that's
  whatever would go past it. With it off, every command runs, as in Codex's
  full access, except what an ask rule names.
- **Protected paths** (below) ask before a file change in every mode but
  `trust`, even `edits`, with the sandbox on or off.
- **MCP servers** run outside the sandbox in every mode, unless you gave one
  walls of its own ([MCP](#mcp-servers)).

### Rules

Rules refine the mode for commands, one per line. Yours, for every
project, are in `~/.config/rein/permissions`, and a project's own in
`.rein/permissions`.

```
allow cargo test
ask   git commit
deny  git push
```

**Yours start as rules that ask before deleting what may not come back:**
`rm`, `rmdir`, `unlink`, `shred`, `rimraf`, `find -delete`, `rsync
--delete`; git's `clean`, `reset --hard`, `checkout --`, `checkout .`,
`restore`, `stash drop`, `stash clear`, `branch -D`, `push --force` (`-f`,
`--delete`, `-d`) and `worktree remove`; `docker rmi` and `prune`, `kubectl
delete`, `terraform destroy` and `dropdb`. They ask in every mode; `auto`,
which never asks, refuses them. rein writes the file the first time it
starts, with a line about each. After that it only adds and removes the
lines you ask it to, so you can edit it too.

- **Ask and deny match broadly.** An ask or deny rule finds its command
  anywhere in a line, by name and whatever its path: after
  `sudo`, `xargs` or `find -exec`, as `/bin/rm`, inside `sh -c '…'`. Its
  other words follow it in order, with anything in between. A long flag
  covers its longer forms, and a short flag the clusters it's in:
  `ask git push --force` finds `git push origin --force-with-lease`, and
  `ask git push -f` finds `git
  push -fu origin`. The question says which rule asked.
- **Allow matches exactly.** Each part of a command must start with an allow
  rule's words, so `allow cargo fetch` doesn't cover `cargo fetch && rm -rf
  x` or `sudo cargo fetch`, and no allow rule covers what rein can't take
  apart, like `$(...)`, redirects or globs.
- **Order.** Deny beats ask beats allow. "Always" allows that command again
  with the same access it had that time, even when an ask rule names it.
- **A project's rules** came with the project, which anyone may have made,
  so its allow rules skip the question only for a command that stays in the
  sandbox. One that needs `outside` gets your keys and always asks. Your own
  rules count wherever a command runs.
- **Rules aren't a sandbox.** A script that deletes files itself, like `make
  clean`, or a command spelt to hide from them, gets past. In `trust`, with
  any deny rule set, a line rein can't take apart asks instead of running
  on a guess.

`/permissions` lists yours and the project's: pick "+ add", or one of yours
to take it out. Or type `/permissions add ask git push` or
`/permissions remove
ask rm`.

### The sandbox

The sandbox is off until you turn it on, in any mode, as in Codex and
Claude Code: `/sandbox`, `--sandbox on|off`, or `sandbox = on`. When it's
on, the operating system holds each command, and everything it starts, to
what it needs, in every mode including `trust`. The model says what a
command needs when it asks to run it:

| Access | The command may |
|---|---|
| `look` | read files, and change nothing |
| `write` | also change files in this folder |
| `network` | also use the internet |
| `outside` | do anything you can: no sandbox |

**What's walled off.** Even with `write` or `network`, a command can't:

- change files outside this folder;
- read anything in your home folder except this folder, git's settings, the
  toolchains and caches builds need, and the folders on your `PATH`. That
  rules out `~/.zshrc` and `~/.env`, where keys are often kept, `~/.ssh`,
  `~/.aws`, other tools' settings and rein's keys, even when a variable like
  `CLAUDE_CONFIG_DIR` or `AWS_SHARED_CREDENTIALS_FILE` moves those folders;
- change what runs code later without anyone choosing to run it:
  `.git/hooks`, `.git/config`, wherever `core.hooksPath` points, `.rein`,
  `.envrc`, and editors' and other agents' settings. Renaming the folder
  that holds one doesn't get round this;
- see keys, tokens and passwords from rein's environment (`*_API_KEY`,
  `*_TOKEN`, a database URL with a password, an SSH agent), or other
  programs' arguments and environments;
- with `network`, reach other programs on this computer, such as Docker's
  socket, which could start a container with your home folder in it, or
  apps, which could act for it outside.

**Tools' records.** Whatever its walls, a command may add to the records
everyday tools keep of their own work, like rtk's stats and its marker that
it warned today. That way `rtk gain` works, and rtk says only once a day
that it can't see its hook. This covers files only, never a link or a
folder, and never the tool's settings beside them, which would change what
the tool does outside the walls next time. rtk's `tee` folder isn't written
from the sandbox.

**When it fails.** A sandboxed command that fails says what its walls were,
so the model knows when to ask for more. Installing into a cache in your
home folder, `docker`, `ssh` and signed commits need `outside`.

**On each system.**

- **macOS:** the system's own sandbox (Seatbelt), at about 10 ms a command.
- **Linux 5.13 or later:** Landlock for files and seccomp for sockets, with
  nothing to install and no user namespaces, so it works on Ubuntu 24.04 and
  in containers. Landlock walls off only whole folders, so what's protected
  inside the project is guarded instead: rein notes it before a command that
  may change it and puts it back after. For a background command, which
  could change it while it runs, it's put back when that command ends.
- **Nowhere else** (Windows, older kernels). There, or where the sandbox
  didn't start, rein says so at startup, commands ask before they run, and
  `auto` refuses them. rein never runs a command unconfined when it meant to
  confine it.

The bar says `sandboxed` while it's on, and `no sandbox` when it's on but
missing.

**Keys.** Other programs of yours can read the environment rein started
with, so rein takes keys and tokens out of it at startup and keeps them
where only it can read them. Commands you let run outside the sandbox still
get them. In anything a command or tool returns, keys rein knows are
replaced with `[a key rein hides]` before the model or the saved
conversation sees it, and `write` and `edit` won't put that in a file in
place of your key. rein sends a key only over https, or plain http to this
computer, and never through a redirect to another server. A crash doesn't
dump rein's memory to a file, and on Linux other programs can't read it.

### Undo

`/undo` puts back what rein did to the project's files: what `write` and
`edit` changed, and what its commands changed, made and removed. It lists
the turns, latest first (with only one, it goes straight to the question),
says what it would put back and take away, and asks first. Going back to
before a turn undoes the later ones too. A file that changed after rein last
touched it, in your editor, say, is left as it is and named. The model is
told what was undone.

`/changes` shows what the conversation changed: each file with the lines
added and taken away. It's the same comparison `/undo` makes, each file as it
was before against how it is now, so what you see is what undoing would put
back. Files that came back as they were don't show. Like `/undo`, it reaches
back 30 turns that changed files: in a longer conversation, what came before
them doesn't show.

- **Commands too.** A command that changed files says so under its line:
  `↳ changed a.rs, made b.rs, removed c.txt`. rein looks at the project's
  files before and after each command that may change them (it skips one
  the sandbox only lets look), and keeps a copy of each that's new or
  changed. After the first look it reads only what changed: with 20,000
  files, the first look takes about 2 s on a Mac and each after about 50 ms.
- **Cheap copies.** They're in `~/.config/rein/undo/`, as clones where the
  file system supports them (APFS, Btrfs, XFS), which take no room until the
  file changes: 500 MB of files took 40 KB.
- **How long.** A conversation keeps its last 30 turns, also after it's
  continued. Another conversation's copies are removed once it's unused for
  a week or ten more have been used since.
- **What it can't.** Only files in the project: not what a command did
  outside it, or to anything but files (a database, a branch, a package).
  What `.gitignore` leaves out, files over 10 MB and symlinks aren't kept.
  In a project with more than 20,000 files or over 1 GB, rein says once
  that it only undoes what `write` and `edit` did.

## Skills

rein reads [Agent Skills](https://agentskills.io): folders with a
`SKILL.md` of instructions for one kind of task.

**Where they come from.**

- Yours, in `~/.agents/skills` (the folder harnesses share) and
  `~/.claude/skills` (Claude Code's).
- Those from the Claude Code plugins you turned on, named as in Claude Code
  (`engineering-skills:senior-backend`).
- A project's, in its `.agents/skills` and `.claude/skills` from where rein
  starts up to the repository's root. They're used once you've said yes to
  them as they are, and a change to any of their files asks again. A task
  run with `-p` doesn't ask.

A folder reached twice through a link counts once. Where two share a name,
the project's wins over yours, and `~/.agents` over `~/.claude`. claude.ai's
skills, which Claude Code keeps in sync with your account, are left to it.
`/skills` shows what was left out and why. Skills are read again before each
message, so one you add or change is there without restarting. The model
hears what changed at the end of the conversation, so its skill list and the
cache behind it stay as they were.

**How the model uses them.** One tool, `skill`, lists every skill and what
it's for, and loads one when a task fits or when you ask. The model gets the
skill's text, where its folder is and what's in it, and reads its files with
the same tool, never outside the folder. A long one comes in parts, and one
already loaded isn't sent again. The list gets 2% of the model's window
(about 5,400 tokens of a 272,000-token one). When the descriptions don't all
fit, the skills you load most keep theirs whole and the rest are shortened
evenly. A skill loaded before a compaction is loaded again after it.

**Calling one yourself.**

- Typing `/` lists the skills you can call, below rein's commands, with what
  each takes. Tab inserts one.
- `/name` at the start of a message calls it with what follows (`/graphify
  src`), as does `/skill:name` when rein has a command of the same name.
  Several at the start load together, each given the rest.
- `/name` anywhere else in a message loads it with the message (`fix the
  login, then /caveman-commit`).
- `rein "/graphify src"` calls one in a task run on its own.
- What you give a skill goes where it says, as Claude Code's skills
  expect: `$ARGUMENTS`, `$ARGUMENTS[0]`, `$name` for one named in its
  `arguments`, and `${CLAUDE_SKILL_DIR}`. Whatever no placeholder took is
  added at the end. A bare `$0` or `$1` is filled only in a skill that
  says it takes arguments, and never in an amount like `$1,234`. In real
  skills they're money or SQL far more often than placeholders, though
  Claude Code fills them anyway.

`disable-model-invocation` hides a skill from the model, and `user-invocable:
false` from you. `/skills` lists them all, most used first. Type a few words
to find one, then put it in the box, turn it off, keep it for you to call, or
turn it on. Call one you turned off and rein asks whether to turn it on and
send your message. Say no, and what you typed is back in the box.

**What a skill can do.** Loading one runs nothing, and its `allowed-tools`
lets nothing run without asking. A name holds only letters, digits and
`-_.:`, and rein removes from a skill's text anything a terminal would act
on. Its scripts run as commands do, so with the sandbox on they run inside
it, where the skill folders can be read but never changed. That includes
Claude Code's, though the rest of `~/.claude` stays hidden.

## Hooks

Hooks are your own commands, run when something happens: before and after a
tool runs, when you send a message, when a turn ends, as a session starts
and ends, before rein asks you, when it needs you while you're away, and
around a compaction. rein runs Claude Code's hooks as Claude Code sets them,
so yours work unchanged. They can be in `~/.claude/settings.json`, in the
plugins you turned on there, or in rein's own `~/.config/rein/hooks.json`,
which has the same shape:

```json
{ "hooks": {
    "PreToolUse": [{ "matcher": "Bash", "hooks": [{ "type": "command", "command": "rtk hook claude" }] }],
    "UserPromptSubmit": [{ "hooks": [{ "type": "command", "command": "codegraph prompt-hook" }] }]
} }
```

A hook gets what happened as JSON on its input and answers as Claude Code's
do. Exit code 2 means no, with the reason on its error output. JSON can
allow a call, make rein ask, change a tool's input, add what the model
should know, or stop the turn. What a hook on your message prints as plain
text is added to the message. rein's tools use Claude Code's names (`Bash`,
`Edit`, `Read`…) and arguments, so a matcher of `Bash` or `Edit|Write`
works.

**Where rein does better.**

- **A rewrite is checked like the model's own call.** rtk turns `git push
  --force` into `rtk git push --force`. Your mode, rules and sandbox decide on
  the rewritten call, and rein's ask and deny rules also check the call as
  the model made it, so a rewrite never gets past them. A hook can spare you
  a question, but never one an ask rule asks.
- **Rewrites don't race.** The hooks on a call run one after another, each
  given the call as the previous one left it. Claude Code runs them at
  once, and the last to finish wins.
- **You see what a hook did,** for example
  `↳ rtk rewrote it, from: git status` or
  `↳ codegraph added about 2300 tokens`. What a hook adds reaches the model
  at the end of the conversation, as the hook's words rather than yours, so
  the cache holds. Anything over 10,000 characters is cut, and rein says so.
  A hook that doesn't work is reported once, and whatever it hooks into goes
  ahead.
- **Your own hooks run outside the sandbox** in every mode, as Claude Code's
  do. In the sandbox, a hook couldn't open its own script in
  `~/.claude/hooks`. Nothing the model runs can plant one, because
  `~/.claude`, rein's folder and the project's `.claude` are out of its
  commands' reach whenever the sandbox is on.
- **Each of a project's hooks waits for your yes, scripts included.** Those
  in its `.claude/settings.json` and `settings.local.json` came with the
  project. As a conversation starts, rein lists each one that's new or
  changed (when it runs, its command, the scripts in the project it names,
  like `.claude/hooks/check.py`) and asks once: "This project comes with a
  hook", with Read it, which shows each script whole and asks again, Trust
  it, and Not now, which leaves it off this time. Your yes is
  kept in rein's folder, and a commit that changes a hook's command or a
  script it names asks again. Claude Code asks once about the folder, so a
  hook from a later commit runs unasked. Codex asks about each hook, but not
  its script. A task run on its own, or a sub-agent, asks nothing and runs
  only what you said yes to. Once you've said yes, a project's hook runs as
  yours do, and `/hooks` turns it on (asking first) or off (taking the yes
  back).

Hooks run with no terminal, in a process group of their own. rein stops one,
with everything it started, when it takes too long (10 minutes, as in Claude
Code; 30 s on your message) or the turn stops. iTerm2's status program for
Claude Code is off unless you turn it on, since rein sets the tab's status
itself. `disableAllHooks` in Claude Code's settings turns them off here too.
rein starts by saying which hooks run. `/hooks` lists each one, where it's
set and how it last did, so you can turn it off or on.

## Memory

The model keeps notes across conversations, per project: a correction you
gave and why, an approach you confirmed, who you are and how you like to
work, what's going on that the project's files don't say, and where to find
something outside it. It saves one with its `memory` tool as it learns it,
and every save is a step you see
(`✓ Memory save rtk-hides-failures
(feedback): judge checks by exit code`). Saving asks nothing in any mode,
since it writes to rein's folder, never the project. "Remember that" saves
a note and "forget that" removes it. It doesn't save code patterns, git
history or what the project's instructions say.

- **The index comes first.** A conversation's first message holds the
  notes' index, a line per note. The model reads a note in full when it
  matters, checking that what it names still exists. At most 200 lines or
  25 KB of it are loaded, as in Claude Code. A note saved mid-conversation
  is in the next one's index, so the start of every request stays the same
  and the cache holds.
- **Claude Code's format.** One plain file per note, with its name,
  description, type and day, and a `MEMORY.md` index, in
  `~/.config/rein/memory/<project>/`. A git worktree's notes go with its
  repository's.
- **Claude Code's notes too.** Those in
  `~/.claude/projects/<project>/memory/` are read, so what it learned carries
  over, but never changed, since its own consolidation would fight rein's
  saves. One of rein's with the same name wins.
- **No background model.** Codex and Claude Code each run one, and Codex's
  drained users' limits.
- **Cleaned both ways.** rein takes the keys it knows out of a note before
  writing it. As notes are read, it takes out anything a terminal would act
  on, or that could break out of their place in the first message.

`/memory` lists both, so you can read one or forget one of rein's. `memory =
off` turns memory off.

## MCP servers

MCP servers give the model more tools: an issue tracker's, a browser's, a
database's.

### Adding one

| Way | What it does |
|---|---|
| `/mcp add playwright` | looks the name up in the official [MCP Registry](https://registry.modelcontextprotocol.io) and lists what it finds, each under its publisher (a GitHub account or a checked domain, like `microsoft/playwright-mcp`). Copies of a popular server share its name and description but seldom its publisher or package, so the one with its own package comes first. |
| `/mcp import` | brings in the servers you set up in Claude Code (yours and this project's), Claude Desktop, Cursor, Codex, VS Code, Windsurf and Gemini CLI. Each is listed with where it's from and what it runs (a variable's name, never its value), marked to bring in unless rein has one of that name, another app has the same name, or the app has it off. |
| `/mcp add context7 npx -y @upstash/context7-mcp` | adds one as you'd start it in a shell. |
| `/mcp add` + a README's JSON | takes the JSON a server's README gives, as it is. |
| `/mcp add` alone, or "+ add" in `/mcp` | asks for a name, a URL, a command or a README's JSON. What you enter isn't shown again or kept for ↑. |
| `/mcp add https://mcp.linear.app/mcp` | adds a server at a URL ([below](#servers-at-a-url)). |

- **Pinned.** rein shows the command it would run, pinned to a version (`npx
  -y @playwright/mcp@0.0.84`), so what runs is what you said yes to.
- **Keys stay out of the file.** A key a server needs is a `${NAME}` of
  yours, asked for in dots as the server starts and never written in your
  `mcp.json`. Each app's way of naming variables (`${env:NAME}`,
  `$NAME`, VS Code's `${input:id}`) is read as rein's `${NAME}`. A server's
  environment is what `env` gives it, and a `${NAME}`'s value is hidden
  wherever it shows up. A line with a key written in (`TOKEN=ghp_…`) isn't
  shown again or kept for ↑, but the key is kept in your `mcp.json`, which
  only you can read. Give the variable's name instead
  (`TOKEN=$GITHUB_TOKEN`) and rein asks for it in dots.
- **Remote first.** When the registry offers both, the server at a URL comes
  first: none of its code runs locally. npm and PyPI packages run
  outside the sandbox, as every server does unless you wall it in. Docker
  images run with Docker, which walls in its own containers.
  `REIN_MCP_REGISTRY` points rein at another registry, such as a company's
  own.

rein starts a server right away and says how it went. The model gets its
tools from your next message.

### Managing them

`/mcp` lists the servers, one line each, with how it's doing (its tool
count, or why it didn't start) and where it runs. Pick one to see its tools,
choose which run without asking, change its walls, restart it, turn it off
(it stays in your file as `"disabled": true`) or on, or remove it. A
project's server you said no to waits there for a yes. Or type
`/mcp allow <name> <tool …|*>` and `/mcp remove <name>`.

**A failure offers its fix where it shows.** If a server needs a variable
you haven't set, rein asks for it in dots and keeps it in
`~/.config/rein/mcp-keys.json`, which only you can read and the sandbox
hides. A value in your environment wins. If a server you walled in fails in
the sandbox, rein asks whether to run it outside. Nothing needs retyping.

**The file.** These commands write `~/.config/rein/mcp.json`, which you can
edit too. rein also reads a project's `.mcp.json`. Both have the shape
Claude Code and Cursor use:

```json
{ "mcpServers": {
    "github": {
      "command": "npx",
      "args": ["-y", "@modelcontextprotocol/server-github"],
      "env": { "GITHUB_PERSONAL_ACCESS_TOKEN": "${GITHUB_TOKEN}" },
      "allow": ["get_issue", "search_issues"]
} } }
```

### Walls, trust and calls

- **Outside the sandbox by default,** in every mode, as in Codex and Claude
  Code, because walls break what servers need (a browser, the keychain,
  another app). `"access"` gives a server walls of its own (`look`, `write`
  or `network`): it reaches the project, and with `network` the internet,
  but not the rest of your home folder, other programs' sockets or rein's
  keys. What `npx` or `uvx` fetches for a walled-in server goes in its own
  folder, `~/.cache/rein/mcp/<server>`. `--access` sets the walls as you add
  one.
- **A project's servers wait for your yes** to exactly what each runs: a
  changed command, argument or variable asks again. A task run with `-p`
  doesn't start one you haven't said yes to. `/mcp` never lets a project's
  server out of its walls, and `/mcp remove` on one stops it and takes back
  your yes.
- **Calls ask** in `ask` and `edits`, because a server's tool may do
  anything the server can reach, and what a server says of a tool (that it
  only reads, say) is only its word. `allow` lets tools run without asking,
  and `"*"` allows all of one server's. `read-only` asks too. `auto` never
  asks, so it runs only allowed ones. Neither a project's `.mcp.json` nor
  its `.rein/permissions` can allow one.
- **A fixed tool list.** The model sees tools as `mcp__<server>__<tool>`,
  listed in a conversation's first request once servers have started (each
  gets 30 s) and unchanged until `/new` or until you add or remove a server.
  So a server can't change what the model was told, and the cache holds.
- **Cleaned answers.** What a server says is cleaned of escape codes and
  characters a terminal doesn't draw. A call may take 5 minutes. A long
  result is cut, and kept whole in a temp file for the model to read in
  parts. A server that stops is restarted on its next call. When one fails
  in the sandbox, the model is told once that the sandbox may be why.

### Tool search

A server's tools can take much of every request, though a task calls a
few. Linear's 70, Playwright's 25 and Chrome DevTools' 32 came to about
25,000 tokens, sent with every message. So once a server's tools would take
more than about 2,000 tokens, rein holds them back. The model gets one tool,
`tool_search`, whose description names each held-back server's tools. With
it the model finds a tool by words (`create issue`) or name
(`select:mcp__linear__create_issue`) and calls it from then on; a step says
`found linear: create_issue`. On a ChatGPT plan with those servers, the first
request went from 27,831 tokens to 3,140. A server with a tool or two stays
shown, since a search would cost more than it saves.

A found tool joins the list after the conversation so far, so the cache
holds. OpenAI's API adds it where it was found (`additional_tools`; its own
tool search isn't accepted on a ChatGPT plan), Anthropic's references it,
and `claude -p` is told of it. Gemini and Chat Completions can't add a
tool later without losing the cache, so they see every tool from the start.
So does a model whose list says it can't search, or that turns search down
(learned per model). A compaction that keeps a call to a found tool keeps
it found. `/mcp` says "found when needed" of a held-back server and lets you
change it: `"tools": "search"` in your file holds a server's tools back
however few, and `"tools": "upfront"` shows them from the start.

### Servers at a URL

```
/mcp add https://mcp.linear.app/mcp
/mcp add ctx --header "CONTEXT7_API_KEY: $CONTEXT7_API_KEY" https://mcp.context7.com/mcp
```

A server at a URL runs none of its code locally. rein talks to it over MCP's
Streamable HTTP (`"type": "http"`, `"url"`, `"headers"`, as Claude Code
writes them) or the older HTTP with SSE (`"type": "sse"`, or `--transport
sse`), which is also tried when a URL turns Streamable HTTP away. rein
reaches it over https, or plain http only on this computer, and follows a
redirect only within the same server. It has no walls, and is named after
its host unless you name it.

**Signing in.** One that wants you to sign in says so, and rein offers it
there: "Sign in to linear?" Yes opens its sign-in page in your browser,
as MCP's spec has it. rein checks that the sign-in server takes PKCE,
registers there as rein (never as another app), and asks for a token for
this server's URL alone. A sign-in server that won't let rein register
takes a client ID you registered yourself. Tokens are kept in
`~/.config/rein/mcp-tokens.json` by URL, and sent to that URL only. A token
about to run out is renewed under a lock every rein window shares, so a
refresh token is spent once. If one can't be renewed, rein asks you to sign
in again from `/mcp`, where you can also sign out. A header of your own
(`Authorization: Bearer $KEY`) is sent as it is, instead of rein's sign-in.
If the server ends a session, rein starts a new one and repeats any call
made in it, which the server didn't take.

## Reins in other tabs

Start `rein` in another terminal tab and the two can talk. It works in any
terminal: rein needs nothing from it but a tab.

```text
 [ ⠹ api · 41s ]  [ ◆ web · needs you ]  [ ✓ review · done 12s ]  [ ○ tests ]
```

**Seeing them.** Each tab's title says which rein it is and what it's
doing: a spinner and the time so far while it works, ◆ while it waits for
you, and how long it took once it's done. Inside each rein, a strip across
the top shows them all live. The line under it runs with light while any
works, turns amber while one needs you, and settles green when they're done
(`strip = off` hides it). iTerm2 3.6+, Ghostty, WezTerm and Windows Terminal
also show a progress bar on the tab, and iTerm2 3.7 a coloured status.
`rein peers` keeps a table of them all open in a tab of its own: pick one
with ↑↓ and press y or n to answer its question without going to its tab.

> Terminal.app adds the folder, the process and more to each tab's title. To
> see only rein's: Terminal → Settings → Profiles → your profile, and on its
> Window and Tab panes uncheck Active process name, Arguments, Working
> directory or document, Path, Shell command name, TTY name, Dimensions,
> Command key and Profile name.

**Talking.**

| Command | What it does |
|---|---|
| `/send api add a /health endpoint` | writes to another rein: an idle one starts on it, a working one reads it at its next step |
| `/delegate api add a /health endpoint` | hands it a task. The result comes back here, with the files it changed |
| `/peers` | all of them: model, state, current step, what each is editing and what went between them. Pick one to write to it, hand it a task or answer its question |
| `/name`, `/workspace` (`--name`, `--workspace`) | who this rein is. A workspace groups tabs on one piece of work, by default the git branch. A name is found in your own workspace first, and as `checkout/api` elsewhere. Renaming a rein renames its tab and its place in every strip, a sub-agent's too. In zellij, a tab rein opened keeps the name it was opened with |
| `--workspace health --worktree` | works in the workspace's own git worktree beside the repository, on a branch of its name, so two workspaces never touch each other's files |
| `/inbox` | what's waiting to be let in |

The model can do this too, with its `peers`, `message` and `delegate`
tools, so "ask the review tab to check this" works. Each tab can run its own
model: a cheap one for tests, another provider's to review what the first
wrote. When a rein changes a file, the others in the same folder hear of
it, and a conversation that has read that file is told.

**Another rein's message is never your word.** The model sees it marked as
another rein's. It can't approve anything, an `@` in it reads nothing, and
what it leads to asks you as usual. A rein lets in only your own processes
that send its token, which is kept in a file only you can read. Anything
more than asking how it's doing must also come from another rein.
Sandboxed commands can't reach the sockets or files at all. A sender that writes too often or repeats itself is
turned away, and once other reins have started 12 turns in a row without
you, what they send waits in `/inbox`. A rein in `trust` mode always holds
what comes in. `peers = hold` does that in every mode, `refuse` takes
nothing, and `off` keeps a rein out. What a rein's own sub-agents send back
or say to it, what the rein that started it says, and the result of a task
it handed out always go in.

### Sub-agents

When the model wants help, its `spawn` tool starts a sub-agent. In `ask`,
`edits` and `read-only`, rein asks first; in `trust` and `auto` it doesn't,
since the sub-agent runs in the same mode and sandbox, and can do nothing a
command couldn't. Then it opens a new tab named for its role (`tests`,
`docs`) and starts a rein there that's already working. `/spawn tests run the suite` starts
one directly, without asking. It shows in your strip as `↳ tests`; watch it,
type into it, answer what it asks, or stop it.

- **Its report shows whole** in the tab that started it, under how it went
  (`✓ tests is done · 14 steps · 2m · changed 3 files`), as the model there
  reads it: up to 16,000 characters, then its start and how much is left.
- **It stays when it's done,** idle, with what it knows, and its tab says
  so. Type in it to carry on, and what it does goes back to the tab that
  started it, which hears of it on its next turn. The model there can also
  hand it more with `delegate`. Finished sub-agents don't count toward the
  four at once.
- **Closing it.** Ctrl-D or `/exit` in it closes its tab. Closing the tab
  yourself asks first in Ghostty, kitty and WezTerm, as rein still runs in
  it. When the rein that started it ends, one you haven't typed into goes
  too.
- **Ending early.** Closing its tab, or `kill`, still sends what it did so
  far: the steps it finished, the one it was in the middle of (which may
  have partly run), the files it changed, its last words and, for one in a
  worktree, where those files are. One killed outright (`kill
  -9`, a crash) is found gone within a second, and the parent's model is
  told.
- **Limits.** At most four work at once. A sub-agent can't start sub-agents
  of its own and runs in its parent's mode. One that changes files
  (`edits`) works in its own git worktree.

**Where tabs open.** rein opens the tab itself in tmux (a new window, in the
background; inside tmux it always uses tmux), kitty (`allow_remote_control
yes`), WezTerm, iTerm2 and Ghostty (macOS asks you once whether that's
allowed), and Terminal.app, as a window (macOS can make it a tab: System
Settings → Desktop & Dock → Prefer tabs when opening documents → Always).
Anywhere else, or over ssh, rein shows the command to paste into a new tab
and copies it. Or set `tab_command` to your terminal's own, with
`{command}` and `{title}` in it. rein never types into a terminal: the new
tab runs `rein --child`, and the sub-agent fetches its task from the rein
that started it.

## Driving rein from another program

`rein serve` is rein with no screen, for an editor, a window of your own or a
script to drive. It speaks the [Agent Client
Protocol](https://agentclientprotocol.com) (version 1) on standard input and
output, one JSON-RPC message a line. What it has to say to people goes to
standard error. It stops when the program closes its standard input.

Sign in first, with `rein login`, or with `/login` from a program that shows
what a command asks. One `rein serve` holds one conversation, in the folder
`session/new` names, so a program that wants another conversation starts
another `rein serve`.

- **Mode.** It starts in the mode your settings say, which is `trust` unless
  you've changed that, so check it before you hand it to anything automatic.
  `session/new` lists the modes, ⚠ marks included, and `session/set_mode`
  changes it between turns.
- **What a project brings.** A project's MCP servers, skills and check stay
  off until you've said yes to them in `rein`, as in a task run on its own. Its
  hooks are asked about as a conversation opens, as the terminal asks, of a
  program that can show what a command asks (below); any other program, and a
  sub-agent, runs only those you've said yes to.
- **Model, effort, mode and sandbox.** `session/new` and `session/load` say
  them as ACP's `configOptions`, with the models by provider as far as they've
  been looked up. `session/set_config_option` changes one between turns, and
  `config_option_update` says when they change, however that happened. Nothing
  is said in the conversation: your control shows the change, and what stopped
  it comes back as the error. A mode or a sandbox is set, and said to be, before
  rein asks whether to keep it. A sandbox turned on where there's none is said
  as a note, since a switch that shows it on can't.
- **Commands.** Any slash command runs as a `session/prompt`, as ACP has it, and
  takes no model. `available_commands_update` lists them, and the skills you
  can call, once a conversation has opened, and again if the skills change.
  What a command says comes as
  `agent_message_chunk` with `"io.github.furkancodes/said": true` under
  `_meta`, one that fails says so there, and the conversation goes on. `/new`,
  `/resume` and `/exit` are for the program: a process holds one conversation.
- **What a command asks.** A program that can show it says so in `initialize`,
  with `"io.github.furkancodes/asks": true` under `clientCapabilities._meta`,
  and gets these requests, each with `sessionId`:
  `_io.github.furkancodes/choose` has `title`, `items` (`name` and `about`),
  `current` and `at`, answered with `picked`, a place in the list;
  `_io.github.furkancodes/read` has `prompt` and `secret`, answered with `text`:
  never show a `secret` one or keep it; and `_io.github.furkancodes/yes` has
  `title`, `detail`, `command` and `answers` (`key` and `says`), answered with
  `key`. `null` means you left it. Any other program is told to give what's
  needed on the line, like `/model gpt-5`.
- **What comes back.** A message of yours that comes back, a command's line or
  a steer, is a `user_message_chunk` with `"io.github.furkancodes/whole": true`
  under `_meta`: ACP's chunks of one role run together otherwise. rein's own
  notes, what a hook did or a background command ending, are
  `agent_thought_chunk`s with `"io.github.furkancodes/note": true`. The line
  that ends a turn that ran steps is one too, and has
  `"io.github.furkancodes/turn"`, the turn to undo back to if it changed files.
  The model's plan comes as ACP's `plan`. The check after edits comes as a step,
  a `tool_call` titled `Check …`, failed if it found new errors, with what it
  printed.
- **Images.** A `session/prompt` can hold ACP's `image` blocks, which go with
  the message as an image you pasted does. `"io.github.furkancodes/label"` under
  a block's `_meta` says what the message calls it, like `[Image #1]`. One rein
  can't show turns the message down and says why: your words aren't sent
  without what they're about. A steer takes words only.
- **Questions.** `session/request_permission` offers what the terminal does,
  in its words: yes; yes and don't ask again, saying for what; no; and no, and
  tell rein what to do instead, which stops the turn. That last is a
  `reject_once` with `"io.github.furkancodes/instead": true` under its `_meta`,
  so a client that doesn't know it still reads a no. Any other answer is a no.
  The request's `_meta` has `"io.github.furkancodes/question"`, what rein asks,
  and `"io.github.furkancodes/why"`, the reason the model gave and the rule
  that asks, if there's either. The step of a file change you're asked about
  has ACP's `diff` in its `content`, with `"io.github.furkancodes/line"` under
  its `_meta`, the line it starts on. Saying don't ask again comes back as a
  note of what it covers.
- **Stopping.** `session/cancel` stops the turn, and whatever it was running.
  The conversation goes on.
- **Steering.** ACP has no way to add to a running turn, so rein has a method
  of its own, `_io.github.furkancodes/steer`, taking `sessionId` and `message`.
  It goes in before the next step, and `initialize` says rein takes it under
  `agentCapabilities._meta`. Sent as a turn is ending, it goes in with your
  next message.
- **While a turn runs.** What can't be done until it ends is turned down with
  the code `-32001`, so a program can tell that from a turn that failed.
- **Branch, limits and the background.** `_io.github.furkancodes/state` says
  the `branch`, your `meters` (`label`, `used`, `said`, `resets` and `warn`) and
  `background`, how many commands are running there, when any of them changes:
  how much is used is the provider's word, and one that says nothing shows
  nothing. A command you stop with `_io.github.furkancodes/stop_task` isn't
  said here: `tasks` shows it stopped.
- **What a box needs.** Each of these takes `sessionId`, and is answered even
  as a turn runs. `_io.github.furkancodes/sent` gives `sent`, what you sent from
  this folder, oldest first, which is what `↑` brings back in a terminal there,
  and what you send here goes in it too, but a line that may hold a key.
  `_io.github.furkancodes/paths` gives `paths`, what `@` can point at, folders
  ending in `/`. `_io.github.furkancodes/background` sends the running command
  to the background, as Ctrl-B does.
- **What's behind a conversation.** These are answered as a turn runs too.
  `_io.github.furkancodes/tasks` gives `tasks`, the commands in the background
  (`id`, `command`, `seconds`, `ended`, `failed` and `last`), and
  `_io.github.furkancodes/stop_task` stops the one you name by `id`.
  `_io.github.furkancodes/room` gives what's taking room as of the last turn:
  `used` and `limit`, `parts` (`name`, `about` and `tokens`), the `biggest`
  outputs (`what`, `tokens` and `stale`), `cache`, and `retiring`, what retiring
  stale output would free.
- **Undo.** These wait for the turn. `_io.github.furkancodes/undo_turns` gives
  `turns`, the latest first (`turn`, `said`, `at`, `changed`, `made` and
  `removed`). `_io.github.furkancodes/undo_plan` takes a `turn` and gives `plan`:
  `putBack`, `takeAway`, `left` and `lost`, the files in each, and how many
  `turns` it undoes. Nothing changes until `_io.github.furkancodes/undo`, which
  takes the same `turn`, and says what it did as a command does.
- **What changed.** `_io.github.furkancodes/changes` takes a `span`, `turn` for
  the last turn that changed anything or `conversation`, and gives `turn`, the
  one to undo, and `files`: each one's `path`, `became` (`changed`, `made` or
  `removed`), `added`, `removed`, `since` if you or another program changed it
  after rein did, which undoing leaves alone, and `unshown` (`binary`, `too_big`
  or `not_kept`) if it has no lines to show. With `lines` true each has its
  `patch`, a unified diff; a file over 1 MB shows none. It's answered during a
  turn too, with what's been changed so far: a command that's running is left
  to finish, and shows once it has.
- **The list of models.** `configOptions` holds the providers looked up so far.
  `_io.github.furkancodes/models` looks up the others, a few seconds at most,
  and `config_option_update` brings them.
- **Signed out.** A program that can show what a command asks gets
  `session/new` turned down with the code `-32002` when you're signed in to
  nothing that runs the model, and the error's `data` has `ways`: each a `way`
  (`chatgpt`, `claude-code`, `key` or `local`), with the `providers` a key or
  your own computer would reach, and `signedIn` for ChatGPT's. `_io.github.furkancodes/sign_in` takes a
  `way` and signs in with no conversation, asking what it needs as `/login`
  would, and answers `signedIn`; `session/new` then opens.
  `_io.github.furkancodes/ways` lists the ways at any time, during a turn too.
- **Keeping the mode.** A program with a switch of its own for keeping the mode
  and sandbox for new conversations says so with
  `"io.github.furkancodes/keeps": true` in `initialize`, and isn't asked as they
  change: the `keep` option, `on` or `off`, writes them to your settings or
  takes them out. It's `on` only when your settings name the mode the
  conversation has, and the sandbox matches, never because rein's own
  defaults do; while it's `on`, a mode or sandbox the program sets is
  written there too. Where the project has a check after edits, the `check` option
  switches it, with its command under `"io.github.furkancodes/command"`.
- **Steps in a few words.** A step that's done has
  `"io.github.furkancodes/summary"` under its `_meta`, what the terminal's line
  says of how it went: `status` and `said`, like `exit 101`, `failed` if it
  did, `added` and `removed` for an edit, and `lines` written or read.
- **Your settings.** `_io.github.furkancodes/config` gives what a settings page
  needs, during a turn too: `file`, where your settings are; `keys`, each with
  what the `file` says and the variable that wins `over` it, if one is set;
  `starts`, what new conversations start with; `choices`; and what the
  conversation found as of the last turn: `rules`, `skills`, `hooks`, `notes`
  and `servers`. `_io.github.furkancodes/set_config` takes a `key` and a
  `value`, or `null` to take it out, and writes it only if rein would start
  with what the file then says; what's wrong comes back as the error.
  `_io.github.furkancodes/rule` takes a `rule`, like `deny curl`, and `add`,
  true or false, for one of yours; it waits for the turn, and counts at once.
- **Flags.** `rein serve` takes what a conversation takes, such as `--mode`,
  `--sandbox`, `-m` and `--name`, but no message: its client sends those.
- **Saved conversations.** `session/list` gives them 100 at a time, the most
  recently used first: those saved in `cwd`, or every project's without it,
  with `nextCursor` while more follow. `session/load` opens one by its
  `sessionId`, in the folder it was saved in, and says it back as updates
  before it answers: what you typed, the model's answers and its steps, and
  what other reins said as `heard` (see below). What rein added for the model
  isn't shown. It needs a `rein serve` that holds no conversation yet.
  The GUI's saved conversations list shows dates in your computer's local
  timezone, not UTC.

### Agents side by side

A program that can run several `rein serve`s says so in `initialize`, with
`"io.github.furkancodes/panes": true` under `clientCapabilities._meta`. Only
then does this rein join the others, as a rein in a tab does: it gets the
`peers`, `message`, `delegate` and `spawn` tools, and what other reins send
can start turns. A client that doesn't say so, like Zed, gets none of this.

- **Starting a sub-agent.** When the model calls `spawn`, rein asks with
  `session/request_permission` in a mode that asks (as in the terminal:
  not `trust` or `auto`), then sends the request
  `_io.github.furkancodes/open_pane` with `name`, `model`, `command`, `args`,
  `cwd` and `env`. Run `command` with `args` in `cwd`, with no shell between
  them and `env` (a list of `name` and `value`) added to its environment, and
  answer once its `session/new` has come back. An error, or an answer before
  then, tells the model the sub-agent didn't start, and its place is free
  again. If you stop the turn first, the same goes.
- **Driving it.** It's a `rein serve` like any other: `initialize` it the same
  way, then `session/new`, which is turned down, and says why, if it can't have
  its task. It starts on its task by itself, and its questions, steering and
  stopping are its own. It reports to its parent when its turn ends, and it
  stays, with what it knows, until you close its input.
- **Closing it early.** Close its input before it's done and its parent hears
  what it did so far.
- **Turns nobody prompted.** What another rein sends, and a background command
  ending, can start a turn with no `session/prompt`, so there's no answer to
  end it. `_io.github.furkancodes/turn_started` says one started, and `_io.github.furkancodes/turn_ended` says how it ended,
  with `stopReason`, or `error` if it failed. Stop one with `session/cancel`.
  A `session/prompt` during one is turned down, as in any turn.
- **What other reins say.** `_io.github.furkancodes/heard` carries `from` and
  `text`. It's never sent as your own message: another rein's words can't
  approve anything.

ACP's own draft for sub-agents lets a client stop one, but not prompt or steer
it, which is why each one is a `rein serve` here.

Not there yet: MCP servers a client passes in `session/new`, which are
ignored. What another rein sends in `trust` mode waits in `/inbox`, which lets
one in as a turn, as anything from another rein is. Past 12 turns in a row that other reins started, what they send starts no
turn: rein says so as a thought, and it goes in with your next message.

## What's left of your limits

When the provider says how much of your limits is used, the bar shows it
after the context, the same way for every provider:

| Provider | What shows |
|---|---|
| Your Claude plan, through Claude Code | the 5-hour and weekly windows, as the CLI reports them with every answer (`5h 28%  ·  7d 75%`), and a model's own weekly window (`7d Opus`) once it gets close |
| OpenRouter | the key's budget or cap (`$1.50 left`), or with no cap, what it spent today (`$0.42 today`); on a free model, today's free requests (`free 12/50`). Checked between turns, at most once a minute. What's left on your account takes a management key to ask, so it isn't shown |
| Your ChatGPT plan | nothing. OpenAI doesn't tell apps that sign in with ChatGPT, so rather than guess, rein shows nothing ([ChatGPT settings → Usage](https://chatgpt.com/settings/usage) has it) |
| Others | nothing yet |

A figure is grey while it's fine, yellow once the provider warns or three
quarters is used, and red at nine tenths or once the limit is reached, with
when it resets (`7d 75% →21:00`). The conversation says once when you're
getting close (`↳ 75% of your 7d limit is used · resets 21:00`), and again
only when you get closer, or close again after a reset. Reaching a limit
says which one, not "check your sign-in". rein never stops
on a figure alone; only the provider's refusal stops anything. A full week
may still be served, from credits for example. A figure past its reset
leaves the bar, another provider's leave when you switch, and in a narrow
window they go before the context gauge. `/status` lists them all.

## Settings

Defaults go in `~/.config/rein/config`, one per line:

```text
model = "gpt-6.1-sol"
effort = "medium"
mode = "edits"
sandbox = on
```

| Setting | Variable | Default | What it is |
|---|---|---|---|
| `model` | `REIN_MODEL` | `gpt-6.1-sol`, or the first provider you're signed in to | the model to use |
| `effort` | `REIN_EFFORT` | the model's own | how hard it thinks |
| `mode` | `REIN_MODE` | `trust` | what runs without asking ([Modes](#modes)) |
| `sandbox` | `REIN_SANDBOX` | `off` | commands in the sandbox ([The sandbox](#the-sandbox)) |
| `search` | `REIN_SEARCH` | `auto` | who searches the web: `auto`, `off`, or `provider/model` |
| `check` | `REIN_CHECK` | asked per project | `off`, or a command to run after edits |
| `memory` | `REIN_MEMORY` | `on` | notes across conversations |
| `compact_at` | `REIN_COMPACT_AT` | `150000` | when to make room, in tokens |
| `max_steps` | `REIN_MAX_STEPS` | `200` | the step limit for a turn nobody watches, or `off` |
| `peers` | `REIN_PEERS` | `accept` | `accept`, `hold`, `refuse` or `off` |
| `strip` | `REIN_STRIP` | `on` | the strip of other reins across the top |
| `bell` | `REIN_BELL` | `on` | the bell when rein needs you and you're away |
| `compress` | `REIN_COMPRESS` | `on` | gzip request bodies |
| `tab_command` | `REIN_TAB_COMMAND` | | your terminal's command to open a tab, with `{command}` and `{title}` |
| `look` | `REIN_LOOK` | `follow` | the window's look: `light`, `dark`, or `follow` your computer's |

A flag wins over the variable, which wins over the file. `/mode` and
`/sandbox` write `mode` and `sandbox` here when you keep what you picked,
`/search` writes `search`, the first run writes `model` when you pick
one of a provider's models, and the window's settings page (`⌘,`) writes
whatever you change there, leaving the rest as it was. `REIN_HOME` moves
rein's whole folder (`~/.config/rein`), where everything above is kept,
readable only by you.

## For contributors

### Layout

| Crate | What it is |
|---|---|
| `crates/rein-llm` | the provider layer: one streaming client for every provider across the four APIs, Sign in with ChatGPT, and signing in to MCP servers at a URL |
| `crates/rein-sandbox` | the walls commands run in: Seatbelt on macOS, Landlock and seccomp on Linux, and what's protected and hidden |
| `crates/rein-mcp` | MCP servers: their files, started in the sandbox or reached over HTTP, cleaning what they say, their tools under rein's names, the Registry and other apps' settings |
| `crates/rein-agent` | the agent loop, the tools, the permission engine, saved conversations, and what the model is told first |
| `crates/rein-cli` | the `rein` command and its full-screen terminal |

[CLAUDE.md](../CLAUDE.md) explains how the code is kept and why each rule is
there. [PRODUCT.md](../PRODUCT.md) says what rein is for and what comes
next.

### Tests

```sh
cargo test
cargo clippy --all-targets -- -D warnings
cargo fmt --check
```

- **On every system.** `.github/workflows/release-builds.yml` runs all
  three, then builds rein, on macOS and Linux, ARM and Intel. It runs when
  you start it by hand, and on a tag such as `v0.1.0`, which must match the
  version in `Cargo.toml` and also makes a draft release with the builds.
  It sets `REIN_TEST_NEEDS_SANDBOX`, so a machine with no sandbox fails
  instead of skipping the sandbox tests.
- **Replayed streams.** The tests replay saved response streams from
  `crates/rein-llm/tests/fixtures`, so they need no network and spend no
  tokens.
- **Screen tests.** `crates/rein-cli/tests/screen.rs` runs the real `rein` in
  a pseudo-terminal against a scripted model that speaks each API rein does,
  and reads the screen through an in-memory terminal emulator. It types,
  scrolls, approves commands, resizes the window, leaves and comes back, and
  checks what you'd see. It also checks the wire: that Claude's thinking
  goes back exactly as signed through compactions, that Gemini's signatures
  go back on their parts, and that a field a server turned away isn't sent
  again.
- **Real providers** (`crates/rein-cli/tests/e2e.rs`) are opt-in, as they
  cost a little. They cover an answer with a tool call from each provider
  you're signed in to (or that's running, for Ollama), Claude's cache across two
  requests, a conversation moved between providers, and Claude through a
  compaction.

  ```sh
  cargo test --test e2e -- --ignored --nocapture --test-threads 1
  ```
- **The sandbox's real walls** are tested on the system they run on, along
  with ways around them: renaming what's protected, symlinks, hard links,
  letter case, other programs' sockets and environments, and terminals.
  `rein-sandbox` and `rein-mcp` have no C in their dependencies, so their
  tests build for Linux on a Mac and run in a container:

  ```sh
  rustup target add aarch64-unknown-linux-musl
  export CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER="$(rustc --print sysroot)/lib/rustlib/aarch64-apple-darwin/bin/rust-lld"
  export CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_RUSTFLAGS="-C linker-flavor=ld.lld -C link-self-contained=yes"
  cargo test -p rein-sandbox -p rein-mcp --target aarch64-unknown-linux-musl --no-run
  docker run --rm -e REIN_TEST_DIR=/work -v "$PWD/target/aarch64-unknown-linux-musl/debug/deps:/t:ro" \
    <an image with python3 and git, a user, and /work> /t/rein_sandbox-<hash>
  ```

  The whole suite runs on Linux too, on a kernel with Landlock: its screen
  tests check the walls through the real binary, and fail where there are
  none, so a container without Landlock runs only the two crates above.

  `rein-mcp`'s tests start a small MCP server written in `sh` in the
  sandbox, so they run there the same way (`/t/rein_mcp-<hash>`). One
  opt-in test fetches a real server from npm and starts it in the sandbox,
  which needs Node and the internet:

  ```sh
  cargo test -p rein-mcp -- --ignored --nocapture a_server_from_npm
  ```
- **Stress the sandbox** when it changes: hundreds of commands at once in
  every kind of access, each checked to work or be walled in. Add `REIN_STRESS_RUNS=5000 REIN_STRESS_AT_ONCE=64` for more,
  and on Linux the same arguments after `/t/rein_sandbox-<hash>`:

  ```sh
  cargo test -p rein-sandbox -- --ignored --nocapture many_commands
  ```
- **Recording a stream.** Recordings contain your account's
  `safety_identifier`; change it to `user-REDACTED` before committing. A test
  fails if you forget.

  ```sh
  rein --record crates/rein-llm/tests/fixtures/<name>.sse "<prompt>"
  ```
