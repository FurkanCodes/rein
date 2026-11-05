<div align="center">

<pre>
█▀▀▄ █▀▀▀ █ █▄  █
█▀█  █▀▀  █ █ ▀▄█
▀  ▀ ▀▀▀▀ ▀ ▀   ▀
</pre>

**A tiny, fast coding agent for your terminal. You hold the reins.**

One 5 MB Rust binary · any model · every step in view

</div>

---

## Quick start

```sh
cargo install --path crates/rein-cli --locked   # from a clone of this repo
rein login                     # your ChatGPT plan; or: rein login anthropic | openrouter | …
rein                           # a conversation in this folder
rein "fix the failing test"    # one task, then exit
rein -c                        # pick up where you left off
```

Everything else is in **[the guide](docs/guide.md)**.

## Features

| | |
|---|---|
| 🧠 **Models** | 4 APIs and 15 providers: OpenAI, Anthropic, Gemini, OpenRouter, Groq, DeepSeek, xAI, Mistral and more, plus Ollama and LM Studio. Switch models mid-conversation, even across providers. |
| 🖥️ **Terminal UI** | Full screen, Markdown answers, diffs before you approve. Enter steers a turn, Esc stops it. `@file` points at code, Ctrl-V pastes an image, Ctrl-O shows any step's full output. |
| 🛡️ **Safety** | Five modes from `trust` to `read-only`. Rules find `rm` behind `sudo` or `sh -c`. An OS sandbox (Seatbelt, Landlock). `/undo` puts back what commands did too. |
| 🔁 **Long work** | Loops are caught, long turns aren't cut. Compaction retires stale output first, and `recall` searches what was compacted. |
| ⚙️ **Background commands** | Dev servers and watchers keep running; the model is told when one ends, so it never polls. |
| ✅ **Checks after edits** | `cargo check`, `tsc`, `go vet`, pyright or mypy after each change, telling the model only the errors it brought in. |
| 🪟 **Sub-agents and tabs** | Each sub-agent gets a terminal tab of its own: watch it, type into it, close it. Its report shows in full where you started it. Reins in other tabs talk with `/send` and `/delegate`. |
| 🔌 **MCP** | Add servers by name from the MCP Registry, import them from 7 apps, or reach them at a URL with sign-in. Big tool sets are found when needed. |
| 🧩 **Skills · hooks · memory** | Agent Skills, Claude Code's hooks as they are, and notes kept across conversations in Claude Code's format. |
| 🔎 **Web search** | Runs in a request of its own, so only the answer joins the conversation. |
| 📊 **Limits** | Your plan's usage in the status bar, only as the provider reports it. |

## Why rein, compared to others

| When… | Elsewhere | In rein |
|---|---|---|
| a sub-agent finishes | Claude Code shows `Done (N tool uses · tokens · time)`; the report itself only under Ctrl-O | The whole report in your window, as the model reads it |
| you want more from a finished sub-agent | Codex and OpenCode make it read-only | It stays: go on in its tab, and its parent hears |
| a sub-agent's tab or pane is closed | Claude Code's lead keeps showing the teammate as running | The parent hears what it did so far, the moment it closes |
| the web is searched | Search inside the conversation (OpenAI's own tool) added ~12,000 tokens, sent again with every request | Only the answer: ~780 tokens |
| errors are checked after an edit | Claude Code and Roo counted an old error that moved as new | Compared by file, code and message, never by line |
| two hooks rewrite one call | Claude Code runs them at once; the last to finish wins | In turn, each given the call as the last left it |
| a cloned project brings hooks | Claude Code trusts the folder; Codex trusts a hook's command, not its script | It asks once for each hook and every script it names, and again if either changes |
| conversations hold images | Codex kept them inline: session folders reached 165 GB | Each image stored once, named in the session |
| memory is kept | Codex's background passes drained users' limits | Saved as the model works, each save a step you see |
| usage is shown | pi showed a cost worked out from a price list | Only the provider's own figures, or nothing |

**Measured:** ~10 ms per sandboxed command · prompt cache at 96–98% · MCP tool search took a first request from 27,831 to 3,140 tokens.

**The official way only:** your ChatGPT plan through OpenAI's Sign in with ChatGPT, a Claude plan through Anthropic's own CLI, or API keys. rein never signs in as another app.

## Where it falls short

- **Young.** Its longest real conversation so far was 9 messages; daily use on itself starts now.
- **Few providers tried for real.** The ChatGPT plan and Claude Code's CLI have been. Gemini, OpenRouter, Anthropic's API and the rest have only run against test servers.
- **A Claude plan only through `claude -p`.** Each step is a fresh CLI run, and its text arrives when the step ends, not word by word.
- **Terminal only.** No IDE extension, desktop or web app.
- **Build it yourself.** No packages yet: you need Rust.
- **No language server.** It finds code with grep and glob, and learns about errors from the project's own check.
- **No usage figures on a ChatGPT plan.** OpenAI doesn't tell apps.
- **Sandbox on macOS and Linux only.** None on Windows. On Linux, some protected files inside the project are guarded and put back rather than walled off.
- **Tabs need a terminal that opens them:** tmux, kitty, WezTerm, iTerm2, Ghostty or Terminal.app. Elsewhere you paste a command. Ghostty, kitty and WezTerm ask before you close a finished sub-agent's tab; Ctrl-D in it doesn't.

## More

- [The guide](docs/guide.md): every feature, in detail
- [PRODUCT.md](PRODUCT.md): what rein is for, and what's next
- [CLAUDE.md](CLAUDE.md): how the code is kept
