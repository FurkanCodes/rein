<p align="center">
  <img src="docs/screenshot/logo.png" alt="rein" width="288">
</p>

<p align="center">
  <a href="#licence"><img alt="Licence: MIT or Apache 2.0" src="https://img.shields.io/badge/licence-MIT%20or%20Apache%202.0-blue"></a>
  <a href="#install"><img alt="Rust 1.95 or newer" src="https://img.shields.io/badge/rust-1.95%2B-orange?logo=rust"></a>
  <a href="#status"><img alt="Tested on macOS" src="https://img.shields.io/badge/tested%20on-macOS-lightgrey?logo=apple"></a>
</p>

rein is an agent harness for the terminal, written in Rust. It reads and
edits your code and runs commands, and you see every step as it happens.
You hold the reins.

![rein in a terminal: asked what a URL slug is, it answers; asked to fix the failing tests, it searches and reads the project, edits slug.py with the lines it changes in red and green, runs the tests again, and says what it changed](docs/screenshot/demo.webp)

## Why rein

Most coding agents are moving toward work you hand off and check on later,
in cloud tasks and background agents. I wanted one I could stay inside:
Enter steers the model in the middle of a turn, Esc stops it, and
sub-agents work in tabs you can read and type into.

If you come from Claude Code, your skills and hooks work as they are, and
`/mcp import` brings your MCP servers over.

rein is one 5 MB binary, and it only runs in your terminal. There's no IDE
extension, desktop app or cloud version.

## Install

There are no packages yet. With Rust 1.95 or newer, from a clone of this
repo:

```sh
cargo install --path crates/rein-cli --locked
```

Then run `rein` in your project's folder. The first time, it asks you to
sign in.

rein works with a ChatGPT plan, a Claude plan (through Anthropic's own
`claude` CLI), or an API key for OpenAI, Anthropic, Gemini, OpenRouter,
Groq, DeepSeek, xAI, Mistral and a few more. Ollama and LM Studio need no
key at all. You can switch models mid-conversation, even to another
provider. rein only signs in the official ways and never borrows another
app's login. [The guide](docs/guide.md#models-and-signing-in) shows how to
set up each one.

## What's in it

- [Sub-agents](docs/guide.md#sub-agents): each one runs in a terminal tab
  of its own, in tmux, kitty, WezTerm, iTerm2, Ghostty or Terminal.app.
  You can watch it, type into it, and keep going with it after it's done.
- [MCP](docs/guide.md#mcp-servers): add a server by name from the
  registry, import the ones you set up in Claude Code, Cursor, Codex and
  others, or connect to one by URL. A server with dozens of tools doesn't
  send them all with every request: the model looks them up when it
  needs one.
- [Skills](docs/guide.md#skills): the ones you already have in
  `~/.claude/skills` and `~/.agents/skills` just work, plugins' too.
- [Hooks](docs/guide.md#hooks): Claude Code's hooks, as you wrote them.
- [Memory](docs/guide.md#memory): notes kept across conversations, in
  Claude Code's format, and you see every save.
- [Checks after edits](docs/guide.md#checks-after-edits): rein runs the
  project's own check (`cargo check`, `tsc`, `go vet`, pyright, mypy) and
  tells the model only about the errors it just introduced.
- [Background commands](docs/guide.md#background-commands): dev servers
  and watchers keep running, and the model hears when one exits instead
  of polling.
- [Web search](docs/guide.md#web-search): runs in a separate request and
  brings back only its answer, so the pages it read don't fill up your
  conversation.
- [Permissions and sandbox](docs/guide.md#what-rein-may-do): rules that
  catch `rm` even behind `sudo` or `sh -c`, modes from `trust` to
  `read-only`, and an optional sandbox (Seatbelt on macOS, Landlock on
  Linux).
- [Undo](docs/guide.md#undo): `/undo` puts back files the model changed,
  including what its commands did.
- [Usage limits](docs/guide.md#whats-left-of-your-limits): your plan's
  usage in the status bar, only when the provider reports it.

## Status

Early. I've just started using it to work on rein itself, on macOS, which
is the only system it has been used on so far. It has run for real on a
ChatGPT plan and through Claude Code's CLI; the other providers are built
to their docs and tested against fake servers so far.

## More

[The guide](docs/guide.md) has everything else: every command and key,
settings, providers, the sandbox. [PRODUCT.md](PRODUCT.md) is where rein is
going. If you want to change the code, read [CLAUDE.md](CLAUDE.md) first.

## Licence

MIT or Apache 2.0, whichever you prefer: [LICENSE-MIT](LICENSE-MIT),
[LICENSE-APACHE](LICENSE-APACHE).
