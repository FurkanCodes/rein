# rein devlog

How rein was built, one stage at a time. Each episode says what was built
and why, and comes with the code as it was then: `git checkout ep02`.

| # | Episode | What it covers |
|---|---|---|
| 01 | [Talking to a model](01-talking-to-a-model.md) | Streaming answers, Sign in with ChatGPT, replay tests |
| 02 | [The agent loop](02-the-agent-loop.md) | Tools, the loop, asking before commands, the prompt cache |
| 03 | Changing files safely | `write` and `edit` with previews, modes, permission rules |
| 04 | A conversation | Interactive mode, Ctrl-C, line editing, saved conversations, `AGENTS.md` |
| 05 | Long conversations | Reading files in parts, compaction, the context gauge, `recall` |
| 06 | A terminal UI | Labels, Markdown, the pinned box, Ctrl-O, slash commands |
| 07 | Small things that matter | Retries, timeouts, the config file, thinking headlines, `plan`, `/undo` |
| 08 | The full-screen shell | Why resizing broke, the full-screen shell that fixed it, and screen tests that keep it fixed (tag on 5c7d96a) |

Coming after that:

| # | Episode | What it adds |
|---|---|---|
| 09 | Your words, kept | History that outlives a run, Ctrl-R, `/copy`, a bell when rein needs you |
| 10 | Finding things | `grep` and `glob` tools |
| 11 | What goes over the wire | Requests gzipped, with a fallback for servers that can't unpack them |
| 12 | Steering | Messages delivered between tool calls |
| 13 | Context you point at | `@file` mentions, grouped reads |
| 14 | A second provider | Anthropic |
| 15 | A real sandbox | macOS Seatbelt, Linux Landlock |
| 16 | MCP | Tools from outside servers |
