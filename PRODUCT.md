# rein: what we're building, and how

The README says what rein does today, CLAUDE.md how its code is kept.
This file says what rein is for, how we decide what to build, and what
comes next.

## What rein is for

**You hold the reins.** rein is an agent harness for coding with a human
in the loop: agents do the work, and you see all of it, steer it, and
decide what matters. Other harnesses are moving toward agents you leave
alone and check on later. rein is for when you'd rather stay in the
work.

What that means in practice:

- **Every agent has a window you can read.** A sub-agent runs in a
  terminal tab of its own, not hidden inside another conversation. You
  can watch it, type into it, answer its questions, stop it, and read
  what it did when it's done.
- **Nothing happens behind your back.** Each step shows as it runs. What
  rein adds to the conversation (a skill loaded, the skills that changed,
  a memory saved) is said where you can see it. What the model is told is
  written in the README.
- **You can always step in.** Esc stops a turn, Enter steers it at its
  next step, and `/undo` puts files back, including what commands did.
- **Walls when you choose them.** The sandbox is yours to turn on or
  off, in any mode, as in Codex and Claude Code: off by default, `/sandbox`
  turns it on for commands. MCP servers and your hooks run outside it, as
  they do there. What came with a cloned project waits for your yes to
  exactly what it is, in every mode. The sandbox stays small:
  its bugs get fixed, and it gets nothing new unless you ask.
- **Your setup comes with you.** Skills, Claude Code's plugins, MCP
  servers from seven apps, and next, hooks and memory: what you set up
  for another harness works in rein without setting it up again.
- **Use what you pay for, the official way.** Your ChatGPT plan through
  OpenAI's sign-in for open-source apps, your Claude plan through
  Anthropic's own CLI, API keys. rein never signs in as another app.
- **Small and fast.** One 4.7 MB binary, about 10 ms a sandboxed command,
  and a prompt cache that holds at 96–98% across a conversation.

## How we build it

Each feature goes the same way:

1. **Research how others do it**: Claude Code, Codex, pi, and whoever
   else does it well, with sources. Then propose how rein does it
   better, as numbered recommendations and a short list of decisions.
2. **Agree** before building.
3. **Build on a fresh branch** from main (`feature/<name>`).
4. **Prove it works**:
   - unit tests;
   - a screen test (the real binary in a terminal, against a fake model);
   - each piece broken on purpose, to watch a test fail;
   - a real run with a real model;
   - numbers measured, not guessed: tokens, milliseconds, binary size.
5. **Report**, then commit and merge only when asked, and reinstall
   after every merge.

"Better" is measured: `react` kept 171 of 211 skills in the old menu
search and 16 in the new; tool search took the first request from 27,831
tokens to 3,140. When rein follows another harness's convention, we
check it against the files people actually have first.

---

## The plan

In the order we'd build them. Testing rein on itself starts now and runs
alongside all of it.

### 0. Testing rein on rein (starts now)

The longest real conversation rein has had is 9 messages and 124 tool
calls, from before the sandbox, MCP and skills existed. It's time to use
it for real.

- **Set up.** Put the feature workflow above, and the habit of judging
  checks by their exit code, in `~/.config/rein/AGENTS.md`. rein reads
  the repo's `CLAUDE.md` (18 KB, under the 32 KB limit) already. Keep the
  sandbox off: in it, `cargo test` runs walled in, where rein's own
  sandbox tests probably can't start one.
- **Use it.** Small and medium rein tasks first, with Claude Code as the
  backup. Note everything that breaks or annoys, with the session file.
  That list decides what comes after this plan.
- **Try what hasn't been tried for real:**
  - a long session through compaction;
  - the `claude-code` provider over a long task;
  - Gemini (AI Studio's free tier);
  - OpenRouter (its free models);
  - Anthropic's API, when there's a key.

### 1. Hooks

**What they are:** your own commands, run when something happens in a
conversation. They can change what's about to happen, add to what the
model sees, or just tell another program.

**Why first:** your daily setup depends on them, and other people's
does too:

- **rtk** (`rtk hook claude`, before each command) rewrites commands so
  their output comes back short, saving tokens.
- **codegraph** (`codegraph prompt-hook`, on each message) adds the code
  your message is about to what the model sees.
- **An iTerm2 status script** (`cc-status`) runs on ten events, from
  session start to session end.

**What we know:** Claude Code's hooks live in `settings.json` under
`hooks`, by event:

- **Events:** before and after a tool, on a message, on a stop, on a
  permission request, at session start and end, and more.
- **Matching:** a matcher picks the tool.
- **Input:** each hook gets JSON on stdin.
- **Output:** an exit code, or JSON that allows, refuses or asks, changes
  a tool's input (how rtk works), or adds context (how codegraph works).

**To research:** the full event list and fields; Codex's `notify` and
its newer hooks; pi's extensions; OpenCode's plugins.

**Better than others:**

- **Your Claude Code hooks work as they are.** rein reads them from
  `settings.json` and from your plugins, the way it reads your skills and
  MCP servers.
- **Your hooks run outside the sandbox** in every mode, as Claude Code
  runs them: they're your setup, not what the model runs.
- **A project's hooks start only after you approve them as they are.**
  Approval is kept for exactly what each one runs; a change asks again.
  Hooks shipped in a cloned repo have been a real problem for other
  harnesses.
- **You can see what a hook did.** Each step shows when a hook changed it,
  for example "rtk rewrote this command".

**Done when:** rtk and codegraph work in rein unchanged; a hook's
rewrite or added context is visible; a project's hook asks first; and the
cache still holds.

### 2. Plan limits in the bar

**What:** next to the context figure in the bar, how much of your plan's
5-hour and weekly limits you've used, and when each resets, for whatever
you're using now. `/status` lists every plan you're signed in to.

**What we know:** limits belong to plans, not to models:

- **A ChatGPT plan** has a 5-hour and a weekly window. Codex shows both,
  read from what ChatGPT's server sends with each answer.
- **A Claude plan** has the same windows. Claude Code's `/usage` shows
  them.
- **An API key** has per-minute rate limits and spend instead, in each
  provider's response headers. OpenRouter also has a credits balance.

**To research:**

- whether rein's requests get the same ChatGPT figures Codex reads;
- whether `claude -p` passes on the Claude plan's figures;
- what each API provider sends;
- how Codex and Claude Code show these, and how they warn as you get near
  a limit.

**Better than others:**

- **Always in view.** It sits in the bar, without opening `/status`.
- **Warned ahead.** You're told before a limit is hit, not by a failed
  request.
- **Your options when a limit is hit.** rein says when it resets and
  offers to switch to another plan or key you have.

**Done when:** the bar shows real figures from your ChatGPT plan, updated
with each answer; a limit about to be hit warns; and a limit hit offers
the alternatives.

### 3. Sub-agents' tabs stay

**Today:** a sub-agent's tab closes once its result is read, unless
you've typed into it.

**Change:**

- **The tab stays when a sub-agent finishes.** It's marked done, with its
  report there to read. You close it when you're done with it.
- **The report also comes back to the tab that started it.** You see it
  there in full, not only the model.
- **Closing a tab before the sub-agent is done still reports.** The
  parent hears what it did so far.

**To research:** how Claude Code shows a sub-agent's work and report,
Codex's sub-agents, and what tmux, iTerm2 and Ghostty allow for a tab
that stays.

**Done when:** a finished sub-agent's tab is still there with its
report; its report shows in the parent's window; and closing a tab early
still sends back what was done.

### 4. Memory across conversations

**What it is:** facts that outlive a conversation:

- that you want research before building;
- that rtk hides failures;
- where the project's dashboards are.

The model saves one as it learns it. A new conversation starts with a
short index of them, a line each, and opens a memory in full only when
it matters. That's how Claude Code's memory works in this repo: the
index is a few hundred tokens, not the conversations it came from.

**Not a handoff.** A handoff (or rein's compaction) carries one
conversation's state into a fresh context. Memory keeps the lessons that
apply to every future conversation. rein has the first already; this is
the second.

**To research:** Claude Code's memory (how it decides what to save and
load), Codex's memories, and others'.

**Better than others:**

- **You see each one as it's saved.** A step line says what was
  remembered. Claude Code saves without saying.
- **`/memory` to manage them.** Read, edit or forget any memory.
- **Plain files you own.** No database.
- **Claude Code's memories carry over.** rein reads them, so what Claude
  Code learned about a project isn't lost.

**Done when:** a correction you give in one conversation is followed in
the next; the index costs few tokens; and every memory is visible and
removable.

### 5. Background commands

**Today:** a command blocks the turn until it finishes or times out (60
seconds, up to 10 minutes when the model asks).

**What background commands do:**

- **The model starts a long command and goes on working.** For example a
  dev server, a watcher or a long test suite.
- **rein answers at once.** It gives the command a name and keeps its
  output in a file.
- **The model checks in as it likes.** It reads what's new, waits for a
  line like "ready on port 3000", or stops the command.
- **The model hears when one ends.** rein tells it when a command exits.

That's how Claude Code runs a dev server and tests against it in one
turn.

**To research:** Claude Code's background commands and how it watches
them, and Codex's long-running terminal sessions.

**Better than others:**

- **Every running command is in view.** Each has a line under the box, and
  Ctrl-O shows its output.
- **You can stop any of them.**
- **They stay in their walls.** They stay sandboxed, and they stop with
  the conversation (the process group goes with it).

**Done when:** the model starts a dev server, tests against it, and
stops it, all in one turn, with you seeing each part.

### 6. Errors after each edit

**What:** after the model edits a file, rein checks it, and the model
hears what broke right away, not three steps later when a build fails.

**LSP**, the Language Server Protocol, is how editors do this:

- **Language servers do the checking.** rust-analyzer for Rust,
  typescript-language-server, pyright for Python, gopls for Go.
- **Editors ask them.** VS Code, Zed and Neovim ask the server what's wrong
  with a file and draw the red underlines.
- **A harness can ask too.** It can start the same servers and ask after
  each edit.

**The simpler way:** run the project's own check (`cargo check`,
`tsc --noEmit`) after edits.

**To research:**

- how OpenCode shows a language server's errors after each edit;
- Claude Code's LSP plugins;
- Crush and Zed's agent;
- what each way costs in time, and how often it's wrong.

**Done when:** a broken edit is caught at the step that broke it, at a
cost we've measured and are happy with.

---

## Open questions

- **Plan limits:** do ChatGPT's figures come to rein's requests as they
  come to Codex's? (First thing to check in 2.)
- **Sub-agent tabs:** should a finished one stay open always, or only
  until its report is read in the parent? (Decide in 3.)
- **Memory:** where should a project's memories live: beside its
  conversations in `~/.config/rein/`, or in the repository, where a team
  shares them? (Decide in 4.)
