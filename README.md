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
  saved with `rein login <provider>`, which asks for it, or `rein login
  <provider> <key>`, which takes it from you there (your shell keeps that line
  in its history). Either way rein checks it first, never shows it again,
  and keeps it in `~/.config/rein/keys.json`, readable only by you. The variable wins over the
  saved key. A gateway in front of Claude takes `ANTHROPIC_AUTH_TOKEN`.
  `OPENAI_BASE_URL`, `ANTHROPIC_BASE_URL` and `GOOGLE_GEMINI_BASE_URL` point
  rein at another server that speaks the same API, like a proxy.
- **OpenRouter, signing in in the browser.** `rein login openrouter` uses
  OpenRouter's official [sign-in for apps](https://openrouter.ai/docs/use-cases/oauth-pkce):
  you say yes in the browser, and OpenRouter makes rein a key of its own,
  billed to your OpenRouter credits, which you can see or delete in its
  settings. `--key` pastes one instead.
- **Nothing, on your own computer.** Ollama and LM Studio need no key.

rein doesn't sign in to Claude or Gemini plans itself: Anthropic and Google
don't allow other apps to sign in with them. For a Claude plan, the
`claude-code` provider runs Anthropic's own CLI (below). rein prints what
pays on every run. With no model named, it uses `gpt-6.1-sol`, or, with no
way to pay OpenAI, the first provider you have a key for (`claude-opus-5-5`,
`gemini-3.8-flash`, ...).

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
request away naming a field rein can do without (`stream_options` on a
server that has none, say), rein sends it again without it and leaves that
field out from then on. An effort level is left out only for the model that
turned it down. What an error can't say, like Mistral's
nine-character tool call IDs, rein knows per provider.

`/model` lists the provider's models and switches to any provider's, in the
middle of a conversation: what was said, and every tool call and result,
carries over in the new provider's format. Reasoning only one provider can
read (OpenAI's encrypted reasoning, Claude's signed thinking, Gemini's
thought signatures) only goes back to that provider. `rein -c` continues with
the model the conversation was left with.

### Claude Code CLI (separate subscription path)

Install the official `claude` CLI, then choose either:

- Run `claude auth login`, then `rein login claude-code` and press Enter to
  use the CLI's stored login.
- Run `claude setup-token`, then `rein login claude-code` and paste the token
  at the hidden prompt. This is the preferred option for unattended use;
  renew it through the CLI when it expires.

Use `rein -m claude-code/sonnet`, `claude-code/opus`, `claude-code/haiku`, or
`claude-code/<full-model-id>`. Inside rein, `/login claude-code` accepts a token
(or an empty answer for the CLI login) and `/model claude-code/sonnet` switches.
You can also supply `CLAUDE_CODE_OAUTH_TOKEN` instead of saving a token. For a
service, configure its PATH to include `claude`; shell profiles aren't loaded.

This is independent of the API-key path. rein
invokes `claude -p`, never sends this token through its HTTP provider, and
strips inherited API keys, endpoint overrides and alternate-provider switches
from the subprocess environment. The token goes to the CLI on a pipe, not in
its environment, where other programs could read it. Each run is in an empty
private folder of its own, with `--setting-sources user`: a project's own
`.claude/settings.json` could otherwise send your token to another server, or
run a command of its choosing. It does not use `--bare`. A recent CLI with
`--safe-mode` and `--json-schema` support is required. Customizations, MCP
servers and built-in tools are disabled: structured responses request rein's
tools, which still go through rein's approvals and sandbox. Each step starts a
fresh subprocess with the conversation; text appears when that step completes,
not token by token. Output is capped at 16 MiB per pipe, quiet subprocesses
time out after five minutes, and a step has a ten-minute ceiling.

Saved tokens live in private `claude-code.json` under rein's config directory.
`rein logout claude-code` removes that configuration; it does not sign the CLI
out, revoke a token, or unset an environment token. Subscription availability,
limits and permitted usage are governed by Anthropic's current terms; this
integration does not guarantee entitlement or a particular token lifetime.

An earlier rein offered `rein login anthropic --oauth`, which signed in to a
Claude plan as Claude Code: with its client ID, headers and system prompt. It
was removed, because rein never signs in as another app. If that sign-in's
`claude-oauth.json` is still in rein's folder, rein deletes it, and says so.

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
- **Effort, per model.** Each model takes its own effort levels, and most
  providers' model lists say which: Sign in with ChatGPT, Anthropic,
  OpenRouter, xAI and DeepSeek list them, and Sign in with ChatGPT and
  OpenRouter also say each model's default. Gemini's levels go by
  generation. `/effort` offers only the levels the model in use takes, names
  its default, and keeps ChatGPT's description of each. You choose a level
  once (`off`, `minimal`, `low`, `medium`, `high`, `xhigh` or `max`), and
  each model gets the closest it has without going over: `max` on a model
  whose top level is `xhigh` sends `xhigh`, and switching back to one with
  `max` sends `max` again. The bar shows the level actually sent. Where the
  list says nothing (an OpenAI API key, Groq, Ollama), every level is
  offered, and one the model turns down is said once and not sent again.
  Claude models from before adaptive thinking get a thinking budget
  instead.
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
rein login anthropic sk-ant-…   # the same, with the key given here
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
| Shift-Tab | the next mode: ask → edits → auto → read-only |
| Enter while rein works | steer it: your message goes into the turn at its next step, and steps rein had planned but not started are skipped. Commands wait for the end of the turn |
| Tab while rein works | send your message after the turn instead. Esc puts what's waiting back in the box |
| Esc or Ctrl-C while rein works | stop that turn; the conversation goes on |
| Ctrl-C at the prompt | clear the line; press again within two seconds to leave |
| Alt-← / Alt-→ | a word left or right |
| Ctrl-O | see every step's full output: ↑↓ scroll, ←→ other steps, `/` search, Esc back |
| Mouse wheel, PgUp / PgDn, Shift-↑ / ↓ | scroll the conversation |
| Drag over text | select it and copy it to the clipboard |
| Ctrl-L | draw the window again |
| `/mode [ask\|edits\|auto\|read-only\|trust]` | show or change what runs without asking (also `/permissions`) |
| `/model [id]`, `/effort [level]` | switch the model, from any provider; change reasoning effort, from the levels the model takes |
| `/login [provider] [key]`, `/logout [provider]` | sign in to a provider without leaving the conversation (a key typed in the box shows as dots; `/login openrouter <key>` takes one given there, and isn't shown again or kept for ↑; OpenRouter and `chatgpt` sign in in the browser), or forget one |
| `/undo` | put back the files rein changed in its last turn (not what commands did) |
| `/new`, `/resume`, `/status` | a fresh conversation (also `/clear`); continue a saved one; what's in use |
| `/compact [focus]` | make room now; the summary keeps `focus` in mind |
| `/copy` | copy rein's last answer as it wrote it, Markdown and all |
| Ctrl-D or `/exit` | leave |

A command given no choice shows what there is to choose from: `/model`
lists every provider's models, `/login` the providers, and `/mode`,
`/effort`, `/logout` and `/resume` theirs. ↑↓ move (PgUp/PgDn a page at a
time), typing narrows the list, as `@` finds files, Enter picks and Esc
changes nothing. The one in use is marked •.

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

## Reins in other tabs

Open another terminal tab, start `rein` there, and the two can talk. It
works in any terminal: rein needs nothing from it but a tab.

```text
 [ ⠹ api · 41s ]  [ ◆ web · needs you ]  [ ✓ review · done 12s ]  [ ○ tests ]
```

Each tab's title says which rein it is and what it's doing: a spinner that
turns and the time so far while it works, ◆ when it waits for you, and how
long it took once it's done. That's in every terminal. Inside each rein, a
strip across the top shows all of them live, in colour, with a line under
it that runs with light while any of them works, turns amber while one needs
you, and settles green when they're done (`strip = off` hides it). iTerm2
3.6 and later, Ghostty, WezTerm and Windows Terminal also show a progress
bar on the tab, and iTerm2 3.7 a coloured status under the title.
Terminal.app's tabs are plain text, and it adds the folder, the process,
its arguments and the window size to each title, which can crowd rein's
out. To see only rein's, go to Terminal → Settings → Profiles, pick your
profile, and on both its Window and Tab panes uncheck Active process name,
Arguments, Working directory or document, Path, Shell command name, TTY
name, Dimensions, Command key and Profile name (whichever each pane has).
In a conversation:

- `/send api add a /health endpoint` writes to another rein. It waits in
  that rein's `/inbox` until you let it in there; with `peers = accept`, an
  idle rein starts on it, and a working one reads it at its next step.
- `/delegate api add a /health endpoint` hands it a task. When it's done,
  its result comes back to this tab, with the files it changed.
- `/peers` shows them all: model, state, current step, what each is
  editing, and what went between them. Pick one to write to it, hand it a
  task, or answer the question it's waiting on.
- `/name` and `/workspace` say who this rein is: `--name` and
  `--workspace` at the start. A workspace is a group of tabs on one piece
  of work: by default the git branch, so tabs on one feature are together.
  A name is found in your own workspace first, `checkout/api` elsewhere.
  Renaming one renames its tab and its place in every strip, a sub-agent's
  too; in zellij, a tab rein opened keeps the name it was opened with.
- `--workspace health --worktree` works in the workspace's own git worktree
  beside the repository, on a branch of its name, so two workspaces never
  touch each other's files.
- `/inbox` holds what's waiting to be let in.

### Sub-agents in tabs of their own

When the model wants help, its `spawn` tool starts a sub-agent: rein asks
you first, then opens a new tab, names it for its role (`tests`, `docs`),
and starts a rein there that's already working. It shows in your strip as
`↳ tests`, you can watch it, type into it, answer what it asks, or stop it,
and its result comes back to the tab that started it, with the files it
changed. Once that result is read, the sub-agent goes and its tab closes,
unless you've typed into it: then it's yours, and stays.

rein opens the tab itself in tmux (a new window, in the background), kitty
(`allow_remote_control yes`), WezTerm, iTerm2 and Ghostty (macOS asks you
once whether that's allowed), and Terminal.app, as a window: its scripting can't make a tab, but macOS
can, with System Settings → Desktop & Dock → Prefer tabs when opening
documents → Always. Inside tmux it
always uses tmux, whatever terminal is around it. Anywhere else, or over
ssh, rein shows the command to paste into a new tab, and copies it; or set
`tab_command` to your terminal's own, with `{command}` and `{title}` in it.

Nothing is ever typed into a terminal: the new tab runs `rein --child`, and
the sub-agent fetches its task from the rein that started it. At most four
run at once, a sub-agent can't start sub-agents of its own, it runs in the
same mode as its parent, and one that changes files (`edits`) works in a git
worktree of its own. A sub-agent whose parent has gone stops.

`rein peers` keeps the same table up in a tab of its own, refreshed every
second. Pick a rein with ↑↓ and press y or n to answer what it asks, without
going to its tab.

The model can do it too, with its `peers`, `message` and `delegate` tools,
so "ask the review tab to check this" works. Each tab can run its own model:
a cheap one for tests, another provider's to review what the first wrote.
When a rein changes a file, the others working in the same folder hear of
it, and a conversation that has read that file is told before its next step.

What another rein sends is never your word. It reaches the model marked as
another rein's message: it can't approve anything, `@` in it reads nothing,
and everything it leads to asks you as usual. A rein lets in only processes
of yours that send its token, which sits in a file only you can read, and,
for anything but asking how it's doing, that are reins themselves. Sandboxed
commands can't reach the sockets or read the files at all. A sender that
writes too often or repeats itself is turned away, and once other reins
have started 12 turns in a row without you, what they send waits in
`/inbox`. What another rein sends waits in `/inbox` for you to let in,
unless you set `peers = accept`, which lets it straight in. A rein in trust
mode, where nothing asks you, holds it even then. What a rein's own
sub-agents send back always goes in. `peers = refuse` takes nothing, and
`off` keeps a rein out of it.

## Settings

Defaults go in `~/.config/rein/config`, one per line:

```text
model = "gpt-6.1-sol"
effort = "medium"
mode = "edits"
compact_at = 120000
max_steps = 200
bell = off
compress = off
sandbox = on
peers = hold
strip = off
tab_command = "my-term --new-tab --title {title} -- {command}"
```

A flag wins over `$REIN_MODEL`, `$REIN_EFFORT`, `$REIN_MODE`,
`$REIN_COMPACT_AT`, `$REIN_MAX_STEPS`, `$REIN_BELL`, `$REIN_COMPRESS`,
`$REIN_SANDBOX`, `$REIN_PEERS` and `$REIN_STRIP`, which win over the file.

## Small things that matter

- A request that fails before anything streams (network trouble, a busy
  server, a rate limit) is retried, 1, 2 and 4 seconds apart.
- Commands are stopped after 60 seconds; the model can ask for up to 10
  minutes for a slow build or test suite.
- A turn goes on until the model is done, however many steps that takes,
  as in Claude Code, Codex and pi: you're watching, and Esc stops it. A
  model going round in circles is caught instead of counted: when the same
  call (or a short cycle of calls) gives the same result three times in a
  row, rein tells it, and at five it stops the turn and says so. A call
  repeated while its result changes, like a build getting further, is
  progress, not a loop. Type `continue` to go on.
- A turn nobody watches (`auto` mode, or `rein "task"`) stops after
  `max_steps` steps, 200 by default (`--max-steps`, or `off`). Its last
  step tells the model, which then says what it did and what's left, so you
  come back to a report, not a turn cut off; a task cut off like this exits
  with an error.
- A long turn is compacted as often as it needs, though never twice
  without a step in between, nor again after a compaction that didn't make
  room.
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

### The sandbox

Commands run in a sandbox: the operating system holds each one, and
everything it starts, to what it needs. The model says what that is when it
asks to run one:

| Access | The command may |
|---|---|
| `look` | read files, and change nothing |
| `write` | also change files in this folder |
| `network` | also use the internet |
| `outside` | do anything you can: no sandbox |

Even with `write` or `network`, a command can't change files outside this
folder, or read your home folder beyond this folder, git's settings, the
toolchains and caches builds need, and the folders on your `PATH`: not
`~/.zshrc` or `~/.env`, where keys are often kept, `~/.ssh`, `~/.aws`, other
tools' settings or rein's own keys, nor such folders when a variable like
`CLAUDE_CONFIG_DIR` or `AWS_SHARED_CREDENTIALS_FILE` moves them elsewhere. Nor
can it change what runs code later
without anyone choosing to run it: `.git/hooks`, `.git/config`, where
`core.hooksPath` points, `.rein`, `.envrc`, editors' and other agents'
settings. Renaming the folder that holds one doesn't get round it. Keys,
tokens and passwords in rein's environment (`*_API_KEY`, `*_TOKEN`, a
database URL with a password in it, the variables your providers file names,
an SSH agent) don't reach the command, nor do other programs' arguments and
environments, and with `network` it reaches the internet but not other
programs on this computer: not Docker's socket, which would let it start a
container with your home folder in it, and not apps, which could do things
for it outside.

A sandboxed command that fails says what its walls were, so the model knows
when to ask for more. Installing packages into a cache in your home folder,
`docker`, `ssh` and signed commits need `outside`.

On macOS the sandbox is the system's own (Seatbelt), and costs about 10 ms
per command. On Linux 5.13 or later it's Landlock for files and seccomp for
sockets, with nothing to install and no user namespaces needed, so it works
on Ubuntu 24.04 and in containers too. Landlock can only wall off whole
folders, though, so on Linux what's protected inside the project is guarded
instead: rein notes it before a command that writes, and puts it back after
(something the command leaves running in the background could still change
it later). Where there's no sandbox (Windows, older kernels), or with
`sandbox = off`, commands ask before they run: rein never runs one
unconfined when it meant to confine it.

Other programs of yours can read the environment rein was started with
(a command in the sandbox can't). So rein takes the keys and tokens out of
it when it starts, keeping them where only it can read them; commands you
let run outside the sandbox still get them. Whatever a command or a tool
says has the keys rein knows replaced with `[a key rein hides]` before the
model or the saved conversation sees it, and `write` and `edit` won't put
that in a file in place of your key. rein sends a key only over https, or
plain http to this computer, and never on through a redirect to another
server. A crash doesn't dump rein's memory to a file, and on Linux other
programs can't read it.

### Modes

`--mode` sets what runs without asking, by what a command needs:

| Mode | File changes | `look` | `write` | `network`, `outside` |
|---|---|---|---|---|
| `ask` (default) | ask | runs | ask | ask |
| `edits` | allowed | runs | runs | ask |
| `auto` | allowed | runs | runs | refused |
| `read-only` | refused | runs | refused | refused |
| `trust` | allowed | runs | runs | runs |

`trust` runs everything without the sandbox, except what deny rules block.
`auto` never asks: it's for leaving rein to work on its own, and what would
go past the sandbox is refused, so the model makes do or tells you what it
needs. File changes to the protected paths above ask in every mode but
`trust`, even in `edits`. Without a sandbox, commands ask in `ask` and
`edits`, and `auto` and `read-only` run none.

Rules in `.rein/permissions` refine this for commands:

```
allow cargo test
ask   git commit
deny  git push
```

Deny beats ask beats allow. The file comes with the project, which may be
anyone's, so an allow rule only spares asking about a command that stays in
the sandbox: one that needs `outside` gets your keys, and always asks you.
It counts only if every part of a command is allowed, so `allow cargo fetch`
doesn't cover `cargo fetch && rm -rf x`; anything rein can't take apart, like
`$(...)`, redirects or globs, no allow rule covers. Deny and ask rules still
find the commands in it. An answer of "always" is for that command with as
much access as that time. Rules decide when rein asks; they aren't a
sandbox, so `deny rm` doesn't stop `/bin/rm`.

## Layout

| Crate | What it is |
|---|---|
| `crates/rein-llm` | Streaming LLM provider layer: one client for every provider, the Responses, Messages, Gemini and Chat Completions APIs, the providers rein knows, Sign in with ChatGPT |
| `crates/rein-sandbox` | The walls commands run inside: macOS's Seatbelt, Linux's Landlock and seccomp, what's protected and hidden |
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
cargo test --test e2e -- --ignored --nocapture --test-threads 1
```

The sandbox's tests run its real walls, on the system they run on, and try
the ways round them: renaming the folder that holds what's protected,
symlinks, hard links, letter case, other programs' sockets, other processes'
environments, terminals. `rein-sandbox` has no C in its dependencies, so its
tests also build for Linux on a Mac (`rustup target add
aarch64-unknown-linux-musl`, linked with the `rust-lld` that comes with Rust)
and run in a container:

```sh
export CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER="$(rustc --print sysroot)/lib/rustlib/aarch64-apple-darwin/bin/rust-lld"
export CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_RUSTFLAGS="-C linker-flavor=ld.lld -C link-self-contained=yes"
cargo test -p rein-sandbox --target aarch64-unknown-linux-musl --no-run
docker run --rm -e REIN_TEST_DIR=/work -v "$PWD/target/aarch64-unknown-linux-musl/debug/deps:/t:ro" \
  <an image with python3 and git, a user, and /work> /t/rein_sandbox-<hash>
```

When the sandbox changes, stress it: hundreds of commands at once in every
kind of access, each checked to work or to be walled in. Add
`REIN_STRESS_RUNS=5000 REIN_STRESS_AT_ONCE=64` for more, and the same
arguments after `/t/rein_sandbox-<hash>` to run it on Linux:

```sh
cargo test -p rein-sandbox -- --ignored --nocapture many_commands
```

Record a real stream with
`rein --record crates/rein-llm/tests/fixtures/<name>.sse "<prompt>"`. Recordings
contain your account's `safety_identifier`; change it to `user-REDACTED` before
committing. A test fails if you forget.
