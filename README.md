# rein

A tiny, fast agent harness. You hold the reins.

rein is an open-source agent harness written in Rust, built to be the
fastest and smallest harness around, and a smart one. It's early: today it holds
a conversation in the folder you run it in, reading, writing and editing files
and running commands, asking you first before deleting what may not come
back (or before more, in the [mode](#modes) you pick).

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
how much of your limits is used where the provider says (see [What's left
of your limits](#whats-left-of-your-limits)), how much came from the cache,
and how long the last turn took. While rein
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
| Ctrl-V | paste an image from the clipboard: `[Image #1]` in the box, and it goes with your message where its label still is (see [Images](#images)) |
| Shift-Tab | the next mode: trust → ask → edits → auto → read-only |
| Enter while rein works | steer it: your message goes into the turn at its next step, and steps rein had planned but not started are skipped. Commands wait for the end of the turn |
| Tab while rein works | send your message after the turn instead. Esc puts what's waiting back in the box |
| Esc or Ctrl-C while rein works | stop that turn; the conversation goes on |
| Ctrl-C at the prompt | clear the line; press again within two seconds to leave |
| Alt-← / Alt-→ | a word left or right |
| Ctrl-O | see every step's full output: ↑↓ scroll, ←→ other steps, `/` search, Esc back |
| Mouse wheel, PgUp / PgDn, Shift-↑ / ↓ | scroll the conversation |
| Drag over text | select it and copy it to the clipboard |
| Ctrl-L | draw the window again |
| `/mode [trust\|ask\|edits\|auto\|read-only]` | show or change what runs without asking, and keep it for new conversations if you like |
| `/sandbox [on\|off]` | run commands in the sandbox, or as you'd run them, in any mode, and keep it for new conversations if you like (see [The sandbox](#the-sandbox)) |
| `/permissions`, `/permissions add <rule>`, `/permissions remove <rule>` | what asks first or is refused: your rules and the project's, then pick "+ add" or one of yours to take out; the same typed (see [Rules](#rules)) |
| `/model [id]`, `/effort [level]` | switch the model, from any provider; change reasoning effort, from the levels the model takes |
| `/search [auto\|off\|model]` | which model searches the web for the model: the one in use as rein decides, one you pick, or none (see [Web search](#web-search)) |
| `/hooks` | your hooks: what each runs on, where it's set and how it last did; pick one to turn it off or on (see [Hooks](#hooks)) |
| `/skills`, `/<skill> [args]`, `/skill:<name>` | your skills, your Claude Code plugins' and the project's, to put one in the box, turn one off, keep it for you to call, or turn it on; call one with your message, or name one anywhere in it (see [Skills](#skills)) |
| `/login [provider] [key]`, `/logout [provider]` | sign in to a provider without leaving the conversation (a key typed in the box shows as dots; `/login openrouter <key>` takes one given there, and isn't shown again or kept for ↑; OpenRouter and `chatgpt` sign in in the browser), or forget one |
| `/mcp`, `/mcp add`, `/mcp import`, `/mcp remove`, `/mcp allow` | the MCP servers and how each is doing, then pick one to change: which tools run without asking, its walls, on or off, or what would get it started; add one by name from the MCP Registry, as you'd start it in a shell, or from the JSON its README gives; bring yours in from other apps; the same changes typed (see [MCP servers](#mcp-servers)) |
| `/undo` | put back what rein did to the project's files since a turn you pick, its commands' changes too (see [Undo](#undo)) |
| `/new`, `/resume`, `/status` | a fresh conversation (also `/clear`); continue a saved one; what's in use, and how much of your limits is used |
| `/compact [focus]` | make room now; the summary keeps `focus` in mind |
| `/copy` | copy rein's last answer as it wrote it, Markdown and all |
| Ctrl-D or `/exit` | leave |

A command given no choice shows what there is to choose from: `/model`
lists every provider's models, `/login` the providers, and `/mode`,
`/permissions`, `/effort`, `/logout` and `/resume` theirs. ↑↓ move (PgUp/PgDn a page at a
time), Enter picks and Esc changes nothing. The one in use is marked •.

Typing narrows the list: every word you type must be in an item's name
or in what's said of it, and names come first. In a name a word's letters
count too, each starting a word or right after the one before, so `vrbp`
finds `vercel-react-best-practices`; never scattered through a
description, where almost any word's letters are somewhere (`react` kept
171 of 211 skills that way, for "pRactices"; now 16). What matched is
marked, a description is shown from where it matched when that's past
the edge, and the box says how many match: `16 of 211`.

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

## What's left of your limits

Where the provider says how much of your limits is used, the bar shows it
after the context, the same way whoever the provider is:

- **Your Claude plan**, through Claude Code: the 5-hour and weekly windows,
  as the CLI says them with every answer (`5h 28%  ·  7d 75%`), and a
  model's own weekly window (`7d Opus`) once it gets close.
- **OpenRouter**: the key's budget or cap (`$1.50 left`), or with no cap,
  what it spent today (`$0.42 today`); on a free model, today's free
  requests (`free 12/50`). rein asks OpenRouter with your key, between
  turns and at most once a minute. What's left on your account takes a
  management key to ask, so it isn't shown.
- **Your ChatGPT plan**: OpenAI doesn't tell apps that sign in with ChatGPT
  how much is used, so nothing shows rather than a guess. [ChatGPT settings
  → Usage](https://chatgpt.com/settings/usage) has it.
- Other providers show nothing yet.

A figure is grey while it's fine, yellow once the provider warns or
three quarters is used, and red at nine tenths or once it's reached, with
when it starts over (`7d 75% →21:00`). Getting close is said once in the
conversation (`↳ 75% of your 7d limit is used · resets 21:00`), and again
only when it gets closer, or close again after it started over. A limit
reached says which one, not "check your sign-in". rein never stops on a
figure alone: a full week may still be served, from credits say; only the
provider's refusal stops anything. A figure past its reset goes from the
bar, another provider's go when you switch, and in a narrow window they go
before the context gauge. `/status` lists them all.

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

- `/send api add a /health endpoint` writes to another rein. If it's idle,
  it starts on it; if it's working, it reads it at its next step.
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
`/inbox`. A rein in trust mode, where almost nothing asks you, always holds
what comes in; `peers = hold` does that in every mode, `refuse` takes nothing,
and `off` keeps a rein out of it. What a rein's own sub-agents send back,
and the result of a task it handed out, always go in.

## Settings

Defaults go in `~/.config/rein/config`, one per line:

```text
model = "gpt-6.1-sol"
effort = "medium"
mode = "edits"
search = "claude-code/sonnet"
compact_at = 120000
max_steps = 200
bell = off
compress = off
sandbox = on
peers = hold
strip = off
tab_command = "my-term --new-tab --title {title} -- {command}"
```

A flag wins over `$REIN_MODEL`, `$REIN_EFFORT`, `$REIN_MODE`, `$REIN_SEARCH`,
`$REIN_COMPACT_AT`, `$REIN_MAX_STEPS`, `$REIN_BELL`, `$REIN_COMPRESS`,
`$REIN_SANDBOX`, `$REIN_PEERS` and `$REIN_STRIP`, which win over the file.
`/mode` and `/sandbox` write `mode` and `sandbox` here when you keep the one
you picked, and leave the rest as it was. The sandbox is off unless it says
`sandbox = on` (or `--sandbox on`).

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

## Images

The model sees an image a tool gives: a screenshot from an MCP server
(Chrome DevTools, Playwright), a `.png`, `.jpg`, `.gif` or `.webp` file
`read` opens, one you name with `@`, or one you paste with Ctrl-V, which
puts `[Image #1]` in the box. Each goes to the model inside its tool's
result, as its provider takes it: in the result for OpenAI's API (a
ChatGPT plan too), Anthropic's and Gemini's; in a message after the tool
results for Chat Completions, whose tool messages take only text; and to
`claude -p` as Claude Code's streamed JSON. A model whose list says it
can't see, or that turns one down (learned per model), is told of it in
words, and it stays in the conversation for one that can.

```
● Read   colors.png   image, 240×120
```

An image is checked once, as it arrives. A PNG bigger than 2,000 pixels a
side or 5 MB, as screenshots are, is shrunk to fit, and the model told how
much, so it can find a point in the original; a JPEG, GIF or WebP goes as
it is when it fits what providers take. One that can't be shown is said
so, never sent: an image a provider turns away would fail every request
after it. Each is kept once, as a file named by what's in it, beside the
project's conversations, which name it and hold none of its bytes. An
image from before your last two messages makes room when the conversation
is compacted, its result saying so and how to see it again.

Ctrl-V reads the clipboard's PNG on macOS, or its TIFF (what a copied
screenshot or an image copied from a browser holds) made a PNG, by a
script macOS runs itself; on Linux, `wl-paste` or `xclip`. Over SSH the
clipboard is on the other computer: put the image in the project and name
it with `@`.

## Skills

rein reads Agent Skills (agentskills.io): a folder with a `SKILL.md`,
instructions for a kind of task. Yours are in `~/.agents/skills`, the
folder harnesses share, and `~/.claude/skills`, Claude Code's; so are the
skills of the plugins you turned on in Claude Code, named as there
(`engineering-skills:senior-backend`). A project's are in its
`.agents/skills` and `.claude/skills`, from where rein starts up to the
repository's root, used once you've said yes to them as they are (a change
to any of their files asks again, and a task run with `-p` doesn't ask).
A folder reached twice through a link counts once; where two skills share
a name, the project's comes before yours, and `~/.agents` before
`~/.claude`. claude.ai's skills, which Claude Code keeps in step with your
account, are left to it. `/skills` says what was left out and why.

They're looked at again before each message: one you add, change or turn
off is there without starting again, and the model is told what changed
at the end of the conversation, so the list it was given, and the cache
behind it, stay as they were.

The model is offered one tool, `skill`, which lists every skill with what
it's for, and loads one when a task fits it, or when you ask it to use
skills: the skill's text, where its folder is and what's in it, whose
files it reads with the same tool, never outside the folder. A long one
comes in parts; one loaded already isn't sent again. Every name stays in
the list, which gets 2% of the model's window (about 5,400 tokens of a
272,000-token one); when the descriptions don't all fit, the skills you
load most keep theirs whole and the rest are shortened alike. A skill
loaded before a compaction is loaded again after it.

Typing `/` lists the skills you may call under rein's commands, with what
each takes; Tab puts one in. `/name` at the start of a message calls it
with what follows (`/graphify src`), as does `/skill:name` where rein has a
command of that name; several at the start load together, each given the
rest. `/name` anywhere else in a message loads that skill with it
(`fix the login, then /caveman-commit`), and `/` there lists them too. A
task run on its own (`rein "/graphify src"`) calls one the same way.

What you give a skill goes where it says, as Claude Code's skills expect:
`$ARGUMENTS` all of it, `$ARGUMENTS[0]` a word of it, `$name` one its
`arguments` name, `${CLAUDE_SKILL_DIR}` its folder; given something
nothing took, it's said at the end. A bare `$0` or `$1` is filled only in
a skill that says it takes something, and never in a sum like `$1,234`:
in the skills people have, it's money or SQL far more often than a
placeholder, which Claude Code fills regardless.

`disable-model-invocation` keeps a skill from the model, `user-invocable:
false` from you. `/skills` lists them all, those you use most first; type
a few words to find one by its name or description, and pick it to put it in the box,
turn it off, keep it for you to call, or turn it back on. Call one you
turned off and rein asks to turn it on and send your message with it;
say no, and what you typed is back in the box.

A skill is someone's text: nothing in it runs because it's loaded, and
its `allowed-tools` lets nothing run without asking. A name holds letters,
digits and `-_.:` only, and what a skill says is cleaned of what a
terminal would act on before you or the model see it. Its scripts run as
commands do: with the sandbox on, in it, which reads the skill folders but
never changes them; Claude Code's are read though the rest of `~/.claude`,
logins and settings, stays hidden.

## Hooks

Hooks are your own commands, run when something happens in a
conversation: before a tool runs and after, when you send a message, when
a turn ends, as a session starts and ends, before rein asks you, when it
needs you while you're away, and around a compaction. rein runs Claude
Code's, as Claude Code sets them, so yours work as they are: those in
`~/.claude/settings.json`, those of the plugins you turned on there, and
rein's own in `~/.config/rein/hooks.json`, in the same shape:

```json
{ "hooks": {
    "PreToolUse": [{ "matcher": "Bash", "hooks": [{ "type": "command", "command": "rtk hook claude" }] }],
    "UserPromptSubmit": [{ "hooks": [{ "type": "command", "command": "codegraph prompt-hook" }] }]
} }
```

A hook is told what happened as JSON on its input, and says back as
Claude Code's do: exit code 2 says no, with why on its error output; JSON
can allow a call, have rein ask, change a tool's input, add what the model
is to know, or stop the turn; and what a hook on your message prints
plainly is added to it, as codegraph's is. rein's tools go by Claude Code's
names for them (`Bash`, `Edit`, `Read`...), with Claude Code's arguments,
so a matcher of `Bash` or `Edit|Write` works.

Where it does better:

- **A rewrite is decided on as the model's own call is.** rtk turns `git
  push --force` into `rtk git push --force`; your mode, your rules and the
  sandbox decide on the call as it runs now, and rein's ask and deny rules
  on the call the model made too, so a rewrite never gets past them. A hook
  of yours may spare you a question, never one an ask rule asks.
- **Rewrites don't race.** The hooks on a tool run in turn, each given the
  call as the one before left it. Claude Code runs them at once, and the
  last to finish wins.
- **You see what a hook did.** A rewritten step says so under its line
  (`↳ rtk rewrote it, from: git status`), and what a hook adds says how much
  (`↳ codegraph added about 2300 tokens`). It goes to the model as the
  hook's words, not yours, at the end of the conversation, so the prompt
  cache holds; at most 10,000 characters, and more is cut and said so,
  never dropped. A hook that doesn't work says so once, and what it was
  for goes ahead.
- **Your hooks run as you'd run them,** outside the sandbox in every mode,
  as Claude Code's do: they're yours, from your settings, and the walls are
  for what the model runs. Walled in, a hook couldn't open its own script
  in `~/.claude/hooks`, and in read-only mode every message was turned
  away. Nothing the model runs can plant one: `~/.claude`, rein's folder
  and the project's `.claude` are out of its commands' reach in every
  walled mode.

- **A project's hooks run once you've said yes to each as it is, its
  scripts too.** Those in its `.claude/settings.json` and
  `settings.local.json` came with the project, which may be anyone's. As
  a conversation starts, rein lists each that's new or changed (when it
  runs, its command, and the scripts in the project it names, like
  `.claude/hooks/check.py`) and asks once, No first. A yes is kept in
  rein's folder, never the project's, and a commit that changes a hook's
  command or a script it names asks again. Claude Code asks once about
  the folder, so a later commit's hook runs unasked; Codex asks about
  each hook, but not its script. A task run on its own, or a sub-agent,
  asks nothing and runs only what you said yes to; `/hooks` turns a
  project's hook on, asking, or off, taking the yes back. Once you've
  said yes, a project's hook runs as yours do.

They run with no terminal, in a process group of their own, stopped with
everything they started when they take too long (10 minutes, as Claude
Code's; 30 s on your message) or the turn stops. iTerm2's status program for Claude Code is
off unless you turn it on, as rein sets the tab's status itself.
`disableAllHooks` in Claude Code's settings turns its hooks off here too.
rein starts by saying in a line which hooks run, and `/hooks` lists each,
where it's set and how it last did, to turn one off or on.

## Web search

The model can search the web with `web_search`, and decides when, as with
any tool: its description says it's for what changes or what the model may
not know (a library's current version, its docs or changelog, an error
message it doesn't recognize, anything after its training) and never for
what's in the project. It asks a question in words, and another request,
to the model chosen for searching, searches with that provider's own
search, reads what it needs and answers with its sources. The conversation
gets that answer, not the pages: asked on a ChatGPT plan, a search added
about 780 tokens to the conversation, where OpenAI's search in the
conversation itself added 12,000, sent again with every request after.

```
● Search  the latest stable Rust release   1 source: blog.rust-lang.org · gpt-6.1-sol, cached
  └ Rust 1.99.1, released 2026-10-15…
```

Who searches is the model in use when its provider can (OpenAI's models,
on a ChatGPT plan too, Anthropic's and Claude Code's), or else the first of
those you can pay for: so a Gemini or local model searches too.
`/search` picks another, say a smaller one of the same provider, or `off`,
and keeps it in your settings as `search`. Gemini's own search isn't used:
Google's terms want its search suggestions shown with what it found, which
a terminal can't.

How far it reaches follows the mode. In trust, where a command can reach
any site anyway, it's live. In the other modes it's OpenAI's index of the
web, so no page is opened as it is now, nor one whose address the model
made: a page couldn't then get the model to send what's in the project out
in a link, past the sandbox's walls. Anthropic's and Claude Code's searches
only search, and open nothing. What a search found is said to be the
web's word, not yours, and the searching model is told not to do what a
page says.

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

### Undo

`/undo` puts back what rein did to the project's files: what `write` and
`edit` changed, and what its commands did, made and removed. It lists the
turns there are to undo, the latest next to the box (with only one, it goes
straight to the question), says what it would put back and take away, and
asks first. Going back to before an earlier turn undoes the later ones too.
A file that changed after rein last did, in your editor say, is left as it
is and named. The model is told what was undone.

A command that changed files says so under its line: `↳ changed a.rs, made
b.rs, removed c.txt`. To know that, rein looks at the project's files
before and after each command that may change them (one the sandbox lets
only look is skipped), and keeps a copy of each that's new or changed since
it last looked. After the first look only what changed is read: with
20,000 files, the first look takes about 2 s on a Mac and each one after
about 50 ms. The copies are in `~/.config/rein/undo/`, readable only by
you, as clones where the file system makes them (APFS, Btrfs, XFS), which
take no room until the file changes: 500 MB of files took 40 KB. A
conversation keeps its last 30 turns to undo, also once it's continued, and
another conversation's copies go once it's a week unused or ten more have
been used since.

It's files in the project only: not what a command did outside it, nor to
anything but files (a database, a branch, a package it installed). What
the `.gitignore` files leave out and files over 10 MB aren't kept (a step
that changed one says `/undo` can't put it back), nor symlinks, and in a
project with more than 20,000 files, or over 1 GB of them, rein says once
that it only undoes what `write` and `edit` did.

### The sandbox

The sandbox is yours to turn on or off, in any mode, as in Codex and Claude
Code: `/sandbox`, `--sandbox on|off`, or `sandbox = on` in the settings.
It's off unless you turn it on, as in Claude Code: commands then run as
you'd run them, and the mode alone decides what asks (see
[Modes](#modes)). On, the operating system holds each command, and
everything it starts, to what it needs, in every mode, trust too. The
model says what that is when it asks to run one:

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

Whatever its walls, a command may add to the records everyday tools keep
of their own work: rtk's stats and the marker that it warned today, so
`rtk gain` works, and rtk says once a day, not with every command, that
it can't see its hook (it can't read Claude Code's settings). Only
as files, never a link or a folder, and never the tool's settings beside
them: changed, those would change what the tool does outside the walls
the next time it runs. rtk's `tee`, the full output of what failed, isn't
written from the sandbox, as a folder of links moved there could take
its next file.

A sandboxed command that fails says what its walls were, so the model knows
when to ask for more. Installing packages into a cache in your home folder,
`docker`, `ssh` and signed commits need `outside`.

On macOS the sandbox is the system's own (Seatbelt), and costs about 10 ms
per command. On Linux 5.13 or later it's Landlock for files and seccomp for
sockets, with nothing to install and no user namespaces needed, so it works
on Ubuntu 24.04 and in containers too. Landlock can only wall off whole
folders, though, so on Linux what's protected inside the project, and a
tool's settings beside its records, is guarded instead: rein notes it
before a command that may change it, and puts it back after
(something the command leaves running in the background could still change
it later). Turned on where there's no sandbox (Windows, older kernels), or
where it didn't start, rein says so as it starts and commands ask before
they run, and `auto` refuses them: rein never runs one unconfined when it
meant to confine it. The bar says `sandboxed` while it's on, and `no
sandbox` when it's on but missing.

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

rein starts in `trust`: everything runs without asking, except what your
rules say to ask about first, which starts as deleting. `--mode`, `mode` in
the settings, or `/mode`, which offers to keep what you pick for new
conversations, set what runs without asking. With the sandbox on, that's by
what a command needs:

| Mode | File changes | `look` | `write` | `network`, `outside` |
|---|---|---|---|---|
| `trust` (default) | allowed | runs | runs | runs |
| `ask` | ask | runs | ask | ask |
| `edits` | allowed | runs | runs | ask |
| `auto` | allowed | runs | runs | refused |
| `read-only` | refused | runs | refused | refused |

With it off, nothing holds a command to what it says it needs, so:

| Mode | File changes | Commands |
|---|---|---|
| `trust` (default) | allowed | run |
| `ask` | ask | ask |
| `edits` | allowed | ask |
| `auto` | allowed | run |
| `read-only` | refused | ask, if they only look; the rest are refused |

`trust` runs every command without asking, in the sandbox when it's on
(and `outside` too, without asking), except what an ask rule names, which
asks, and what a deny rule blocks. What the model reads (a web page, an
issue, a file in a cloned repository, an MCP server's answer) can lead it
to run any command with your keys and the network, and the rules are about
mistakes, not about that: for work you'd rather not watch, turn the
sandbox on, and its walls hold whatever the model is told. `auto` never
asks: it's for leaving rein to work on its own, and what would ask is
refused instead. With the sandbox on, that's what would go past it, so the
model makes do or tells you what it needs; with it off, every command runs,
as in Codex's full access, but what an ask rule names. File changes to the
protected paths above ask in every mode but `trust`, even in `edits`, with
the sandbox on or off. An MCP server runs outside the sandbox in every mode,
unless you gave it walls of its own (see [MCP servers](#mcp-servers)).

### Rules

Rules refine the mode for commands, one per line: yours, for every
project, in `~/.config/rein/permissions`, and a project's own, in its
`.rein/permissions`:

```
allow cargo test
ask   git commit
deny  git push
```

Yours start as rules that ask before deleting what may not come back:
`rm`, `rmdir`, `unlink`, `shred`, `rimraf`, `find -delete` and
`rsync --delete`; git's `clean`, `reset --hard`, `checkout --` and
`checkout .`, `restore`, `stash drop` and `stash clear`, `branch -D`,
`push --force` (`-f`, `--delete`, `-d`) and `worktree remove`; and
`docker rmi` and `prune`, `kubectl delete`, `terraform destroy` and
`dropdb`. They ask in every mode; `auto`, which never asks, refuses them.
`/permissions` lists your rules and the project's: pick "+ add" to add one,
or one of yours to take it out, or type `/permissions add ask git push`
and `/permissions remove ask rm`. rein writes the file the first time it
starts, with a line about each, and after that only adds and takes out
the lines you ask it to, so you can edit it too.

An ask or deny rule finds its command anywhere in a line, by its name past
any path and whatever the letters' case: after `sudo`, `xargs` or
`find -exec`, as `/bin/rm`, inside `sh -c '…'`. Its other words come
after it, in order, wherever they are, a long flag covering its longer
forms and a short one the clusters it's in: `ask git push --force` finds
`git push origin --force-with-lease`, and `ask git push -f` finds
`git push -fu origin`. The question says which rule asked. An allow rule
is strict: each part of a command has to start with an allow rule's words,
so `allow cargo fetch` doesn't cover `cargo fetch && rm -rf x` or
`sudo cargo fetch`, and anything rein can't take apart, like `$(...)`,
redirects or globs, no allow rule covers. Deny beats ask beats allow. An
answer of "always" is for that command with as much access as that time,
even when an ask rule names it.

A project's file comes with the project, which may be anyone's, so its
allow rules only spare asking about a command that stays in the sandbox:
one that needs `outside` gets your keys, and always asks you. Yours count
wherever a command runs. Rules decide when rein asks; they aren't a
sandbox: a script that deletes files itself, like `make clean`, or a
command spelt to hide from them gets past. In `trust`, with any deny rule,
a line rein can't take apart asks rather than run on a guess.

## MCP servers

MCP servers give the model more tools: an issue tracker's, a browser's, a
database's. Add one in the conversation by its name:

```
/mcp add playwright
```

rein looks it up in the official [MCP Registry](https://registry.modelcontextprotocol.io)
and lists what it finds, each under who published it: a GitHub account or
a domain the registry checked, like `microsoft/playwright-mcp`. Copies of a
popular server often share its name and its description, but seldom its
publisher or its package, so the one whose package is its own comes
first. Pick one, give it what it needs that isn't a key, and rein shows
the command it would run, pinned to a version (`npx -y
@playwright/mcp@0.0.84`), so what you said yes to is what runs. A key it
needs is a `${NAME}` of yours, asked for in dots as it starts, and never
written in your `mcp.json`. One reached at a URL comes first when the
registry offers both: none of its code runs here. npm and PyPI packages
run outside the sandbox, as every server does unless you wall it in
(below); Docker images run with Docker, which
walls in its containers itself and which the sandbox can't reach. `REIN_MCP_REGISTRY`
points rein at another registry, as a company's own.

```
/mcp import
```

brings in the servers you set up in Claude Code (yours, and this
project's), Claude Desktop, Cursor, Codex, VS Code, Windsurf and Gemini
CLI: each is listed with where it's from and what it runs (a variable's
name, never its value), marked to bring in unless rein has one of its name,
another app has the same name, or the app has it off. Each app's way of
naming your variables (`${env:NAME}`, `$NAME`, VS Code's `${input:id}`) is
read as rein's `${NAME}`.

Or add one as you'd start it in a shell, or paste the JSON a server's
README gives after `/mcp add`, as it is:

```
/mcp add context7 npx -y @upstash/context7-mcp
```

rein starts it at once and says how it did, and the model is offered its
tools from your next message. `/mcp add` alone, or "+ add" in `/mcp`,
asks for any of these: a name to look up, a URL, a command, or a
README's JSON pasted as it is, which isn't kept for ↑ or shown again.

`/mcp` lists the servers to pick one and change it: choose the
tools that run without asking, change its walls, restart it, turn it off
(it stays in your file, `"disabled": true`) or on, or remove it. A
project's server you said no to waits there for a yes. Typed, the same is
`/mcp allow <name> <tool …|*>` and `/mcp remove <name>`.

When a server doesn't start, rein says why and offers what would fix it,
there and then. One that needs a variable you haven't set (`${NAME}`) has
it asked for, in dots, as rein starts or as you add it: it's kept in
`~/.config/rein/mcp-keys.json`, which only you can read and the sandbox
hides, and filled in when your shell hasn't it. Your environment's value
wins. One you walled in that fails in the sandbox is asked about running
outside it, which a yes writes in your file. Nothing needs typing again.

These write `~/.config/rein/mcp.json`, which you can edit too. rein also
reads a project's `.mcp.json`. Both have the shape Claude Code and Cursor
use:

```json
{ "mcpServers": {
    "github": {
      "command": "npx",
      "args": ["-y", "@modelcontextprotocol/server-github"],
      "env": { "GITHUB_PERSONAL_ACCESS_TOKEN": "${GITHUB_TOKEN}" },
      "allow": ["get_issue", "search_issues"]
} } }
```

A line that writes a key in as it is (`TOKEN=ghp_…` rather than
`$GITHUB_TOKEN`) isn't shown again or kept for ↑. The key is kept in your
`mcp.json`, which only you can read. Leave it out and give the variable's
name instead (`TOKEN=$GITHUB_TOKEN`): rein asks for it in dots.

A server runs outside the sandbox, as in Codex and Claude Code, whatever
the mode and whether the sandbox is on for commands: walls break what
servers need (a browser, the keychain, another app), and one that came with
a project waits for your yes to what it runs instead. `"access"` gives a
server walls of its own, whatever the mode: `look`, `write`, or `network`,
the way a command with that access runs, reaching the project (and with
`network` the internet) but not the rest of your home folder, other
programs' sockets, or rein's keys; or `outside`, which is what none means.
What `npx` or `uvx` fetch for a walled one goes in a folder of the server's
own, `~/.cache/rein/mcp/<server>`, which nothing else may write. `/mcp`
changes a server's walls, and `--access` before the command sets them as
you add one. A project's server is never let out from `/mcp`: the
project doesn't decide that. Its environment is what `env` gives it. `${NAME}` is a
variable of yours, filled in as it starts, and its value is hidden wherever
it shows up.

A project's `.mcp.json` came with the project and may be anyone's. So rein
asks before it starts one of its servers, showing exactly what it runs, and
keeps your yes for the server as it is: a changed command, argument or
variable asks again. A task run with `-p` doesn't start one you haven't said
yes to. `/mcp remove` on one of a project's servers stops it and takes back
your yes.

The model sees a server's tools as `mcp__<server>__<tool>`. They're listed
in a conversation's first request, once the servers have started (each gets
30 s), and stay as they are until `/new`, or until you add or remove a
server. So a server can't change what the model was told mid-conversation,
and the prompt cache isn't lost. A call
asks first in `ask` and `edits`: a server's tool may do anything wherever
the server reaches, and what a server says of a tool (that it only reads,
say) is only its word. `/mcp allow`, or `allow` in your own file, lets
tools run without asking, and `"*"` allows all of one server's. `read-only`
asks too, as a lookup (codegraph's, say) is what it's for; `auto`, which
never asks, runs only those. Neither a project's `.mcp.json` nor its
`.rein/permissions` can allow one.

### Tool search

A server's tools can take much of every request, though a task calls a
few of them: Linear's 70, Playwright's 25 and Chrome DevTools' 32 came to
about 25,000 tokens, sent again with each message. So once a server's
tools would take more than about 2,000 tokens, rein holds them back. The
model is shown one tool instead, `tool_search`, whose description names
each held-back server's tools on a line. It finds what it needs by words
from what they do (`create issue`) or by name
(`select:mcp__linear__create_issue`), and calls them from then on: a step
says `found linear: create_issue`. On a ChatGPT plan with those servers,
the first request went from 27,831 tokens to 3,140. A server of a tool or
two stays shown, as a search would cost more than it saves.

What's found joins the model's tools after the conversation so far, so the
prompt cache holds. OpenAI's API adds them where they were found
(`additional_tools`; its own tool search isn't taken on a ChatGPT plan),
Anthropic's references them as its tool search does, and `claude -p` is
told of them from then on. Gemini and Chat Completions providers can't add
a tool later without losing the cache, so they're shown every tool, and so
is a model whose list says it can't find them, or that turns it down
(learned per model). A compaction that keeps a call to a found tool keeps
it found. `/mcp` says "found when needed" of a server held back, and
changes it: `"tools": "search"` in your file holds a server's tools back
however few, and `"tools": "upfront"` shows them from the start.

What a server says is cleaned before you or the model see it: escape codes,
and characters a terminal doesn't draw, are taken out. A call may take 5
minutes. A long result is cut, and kept whole in a file in the temp folder
for the model to read in parts. A server that stops is started again on its
next call. `/mcp` lists each server once, a line each, to pick from: how
it's doing (its tools counted, or why it didn't start) and where it runs
or is reached. Pick one to see its tools by name, and why in full. When one of a server's calls fails in the sandbox, the model
is told once that the sandbox may be why, and that you can let it out from
`/mcp`.

### Servers at a URL

```
/mcp add https://mcp.linear.app/mcp
/mcp add ctx --header "CONTEXT7_API_KEY: $CONTEXT7_API_KEY" https://mcp.context7.com/mcp
```

A server at a URL runs none of its code here: rein talks to it over MCP's
Streamable HTTP (`"type": "http"`, `"url"` and `"headers"` in your file,
as Claude Code writes them), and it has no walls. Named for its host if
you don't name it. rein reaches one over https, or over plain http only on
this computer, and follows a redirect only on the same server. A server
that speaks only the HTTP with SSE that came before Streamable HTTP is
spoken to over that (`"type": "sse"`, or `/mcp add --transport sse
<url>`): one stream kept open for its answers, and its messages sent where
the stream says, which must be on the same server. A URL that turns
Streamable HTTP away is tried over SSE too, as MCP's spec says.

One that wants you to sign in says so as it's added or starts, and rein
offers it there: "Sign in to linear?" Yes opens its sign-in page in your
browser, as MCP's spec has it. The server names its sign-in server; rein
checks it takes PKCE, registers itself there as rein (never as another
app), and asks for a token for this server's URL alone. A sign-in server
that won't let rein register takes a client ID you registered with it
yourself. Tokens are kept in `~/.config/rein/mcp-tokens.json`, which only
you can read and the sandbox hides, by the server's URL, and sent to that
URL only. One about to run out is renewed before a request, under a lock
every rein window shares, so a refresh token is spent once; one that can't
be renewed asks you to sign in again from `/mcp`, which also signs you
out. A header of yours (`Authorization: Bearer $KEY`) is sent as it is,
and rein's sign-in isn't. A session the server ends is started again, and
a call made in it made again: it wasn't taken.

## Layout

| Crate | What it is |
|---|---|
| `crates/rein-llm` | Streaming LLM provider layer: one client for every provider, the Responses, Messages, Gemini and Chat Completions APIs, the providers rein knows, Sign in with ChatGPT, and signing in to MCP servers at a URL |
| `crates/rein-sandbox` | The walls commands run inside: macOS's Seatbelt, Linux's Landlock and seccomp, what's protected and hidden |
| `crates/rein-mcp` | MCP servers: their files, started in the sandbox or reached over Streamable HTTP, what they say cleaned, their tools under rein's names; what the MCP Registry lists and other apps' settings have, read into servers to add |
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
cargo test -p rein-sandbox -p rein-mcp --target aarch64-unknown-linux-musl --no-run
docker run --rm -e REIN_TEST_DIR=/work -v "$PWD/target/aarch64-unknown-linux-musl/debug/deps:/t:ro" \
  <an image with python3 and git, a user, and /work> /t/rein_sandbox-<hash>
```

`rein-mcp` has no C in its dependencies either, and its tests start a small
MCP server written in `sh` in the sandbox, so they run there the same way
(`/t/rein_mcp-<hash>`). One opt-in test fetches a real server from npm and
starts it in the sandbox, with Node and the internet:
`cargo test -p rein-mcp -- --ignored --nocapture a_server_from_npm`.

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
