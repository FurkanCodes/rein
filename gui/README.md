# rein's window

All your agents side by side, each one to read and steer. It's a workspace of its own, so nothing of GPUI's reaches rein's dependencies or its checks. It isn't released: it's headed for a macOS beta, and `PRODUCT.md`, section 7, says what's left.

- `replay/` records `rein serve`, makes the workloads, and plays them back as an agent.
- `transcript/` is a conversation as a window holds it: rows, block cutting, selection, approval questions, the steps of a turn, the context and the modes, with no GPUI in it.
- `window/` is the GPUI part, and measures itself.
  - `look.rs` is every colour, by the design's names, light and dark. Nothing else says a colour.
  - `parts.rs` is the one button (`Press`), and what's drawn rather than written: gauges, the strip, the working mark, the wordmark.
  - `panes.rs` is the desk and the list of agents; `pane.rs` is one agent, `pane/rows.rs` its conversation on the rail, `pane/foot.rs` the question, the box and the bar.
- `stubs/` holds an empty crate: GPUI's test support names one that was never published, and without it a window can't be tested.
- `gates/run.py` runs the gates and samples CPU and memory from outside.

## Run it

```sh
cargo run -p rein-window --release
```

It opens with nothing on the desk: `⌘N` starts an agent in a folder, and Saved conversations opens one you had. Each `--agent 'rein serve --mode ask'` starts one with it. The command is split on spaces, so a path with spaces in it won't work, and `rein` takes the folder you run this from as its project.

It wears your Mac's light or dark look, and `--look light` or `--look dark` pins one.

## What the bar shows

Only what the agent has said: a cell with nothing behind it isn't drawn, and a change in rein, however it was made, shows here. A cell runs the command for what it shows, so the model, the effort and the mode each open their list, and `⇧⇥` goes on to the next mode. The `+` waits for a way to attach files.

## Commands

Everything the terminal's `/` does works here: `rein serve` runs the command, says what it says into the conversation, and asks what it would ask, so a list to pick from, a line to type and a question all come up over the box. Type `/` for the list: `⏎` runs the one picked, `⇥` puts it in the box to add to, and a command waits for a turn to end. `/new`, `/resume`, `/exit` and `/copy` are the window's own: a pane is a conversation. So are `/undo`, a sheet, and `/tasks`, a panel.

The box goes by the terminal's rules, and what you type goes in it wherever you last pressed, unless a list or a sheet that's up has the keys. The conversation's text keeps them for `⌘C` until you type. `↑` and `↓` bring back what you sent from this folder, the same list a terminal there has, and `^R` finds one by its words. `@` lists the project's files: `⇥` puts one in, and `⏎` does unless it's typed out. While a turn runs, `⏎` steers it and `⇥` sends after it, which a command and a message with an image always do, as does what a running command can't take in; what's waiting shows over the box, a press brings it back, and stopping the turn brings it all back. `^B` sends the running command to the background, `^O` opens every step's output, and a line ending in `\` goes on to the next.

`+` adds files: an image goes with your message wherever it is, and anything else is named with `@`. So does pasting or dropping one. A TIFF, all that some copied images are, goes as a PNG.

The bar's model and mode open lists over the box, the model's with its effort, the mode's with the sandbox. What you pick shows in the bar, and nothing's written in the conversation unless it fails. A pick during a turn waits for its end. In a narrow pane they give up room, and what acts stays in reach. The context gauge opens a panel of what's taking room, a cell shows up with what's running in the background and opens Tasks, and a turn that changed files ends in a line that can undo it. `⌘K` goes to an agent or runs a command.

A question asks in the terminal's words. A file change shows all of its lines, and a command why it's asked: the model's reason, and the rule that asks. Always says what it covers when the pointer rests on it, and in a note once pressed. `esc` is no, and tell rein what to do instead, which stops the turn.

A question takes the keys only if the box is empty and the pane is yours, and `y`, `a`, `n` and `esc` show only while they work. It ignores keys and clicks for its first 400 ms: what lands as it appears was meant for something else, and the second press of a double click on Allow would otherwise answer the next question unread. A press also counts only for the question it was drawn for. Any other key goes to the box, and the keys with it, so a word that happens to contain a `y` can't say yes.

The mode in the bar is the one the agent says it's in: a change it turns down leaves the bar as it was, and says why in the conversation.

## Testing it

```sh
cargo test
```

`window/src/behaviour.rs` and the files under it run real panes in a real window with no GPU behind it: what comes of what an agent says, and of your keys and clicks. The gates below measure speed and these don't.

## The gates

```sh
cargo build --release -p rein-window -p rein-replay
python3 -I gates/run.py            # every gate, about 8 minutes
python3 -I gates/run.py typing     # or some of them
```

Windows float at the top right while it runs. They leave your keyboard alone. A window has to be on screen to be measured: GPUI draws nothing while it's hidden or the display is off, and a gate then reads as fast and light, so `run.py` stops if the display goes off. `REIN_GATES_FOCUSED=1` runs in a window that takes your keyboard. `REIN_WINDOW` picks another build of the window, `REIN` another rein.

The pass marks come from `reports/Fast Rust desktop GUI options.md`. They are judgement, not published numbers.

## Results

Measured on one M5 at 120 Hz, on battery, with other work running, one run of each gate, in a window of 1156 by 780. Keys are p50 / p99 / max from a key going in to the frame that shows it. Memory is the physical footprint, what Activity Monitor shows.

| Gate | Result | State |
|---|---|---|
| Typing with 3 panes replaying, one of 10,000 messages | keys 6.8 / 13.2 / 14.7 ms, longest stall 16.1 ms, 35% CPU, 200 MB | Pass |
| Idle, nothing focused | 2 frames in 60 s, 0.32 wake-ups/s, 0.0% CPU, 172 MB | Over the mark: the ages in the list tick |
| Idle, caret in a box | 2 frames/s, 6.8 wake-ups/s, 0.1% CPU, 215 MB | Over the mark: a caret draws |
| A pointer crossing the window for 6 s | 8 frames more than idle, 0.0% CPU, 171 MB | No mark set |
| Open and scroll 10,000 messages | opens in 58 ms, 120 fps, 1.7 ms/s of hitches against Apple's line of 10, longest frame gap 13.8 ms, 215 MB | Pass |
| Scrolling back while an answer streams | 120 fps, 1.6 ms/s of hitches, longest stall 8.9 ms, 193 MB | Pass |
| Resizing while an answer streams | keys 3.5 / 11.7 / 12.5 ms, longest stall 17.5 ms, 178 MB | Pass |
| A 200 KB answer | keys 6.1 / 12.9 / 13.8 ms, longest stall 9.0 ms, 185 MB | Pass |
| 20,000 lines of code | keys 5.0 / 9.4 / 9.8 ms, longest stall 6.1 ms, 239 MB | Pass |
| 500-row table, to its end | keys 7.0 / 11.4 / 12.3 ms, one stall of 69 to 91 ms as it ends, 204 MB | Pass at p99, see below |
| Burst of 100 KB/s | keys 7.3 / 14.9 / 26.0 ms, longest stall 17.0 ms, 176 MB | Pass |
| One byte at a time | keys 6.0 / 12.7 / 13.5 ms, longest stall 9.5 ms, 162 MB | Pass |
| Megabytes of tool output | keys 7.9 / 11.0 / 11.1 ms, longest stall 4.3 ms, 137 MB | Pass |
| Selection across rows, then scrolled away | the toolkit's goes from 928 bytes to 0, the transcript's stays at 943. Shift-click took 2.5 MB, 37,830 lines, in 2.4 ms | Pass, with the limits below |
| `rein serve` holding 10,000 messages | 73 MB as it opens, 39 to 73 MB after 30 s, up from 2.8, for 13.8 MB of text; opens in 283 to 673 ms | No mark set |
| macOS 26 freeze | none on macOS 26.5.2 | Pass |
| Build loop | clean 48 s debug, 78 s release; a one-line change 1 to 3 s | Pass; hot reload untried |
| Upkeep | 0.9.0 is the newest release, so there's nothing to upgrade to. This code built on 0.8.1 unchanged, before the last fixes: a week's drift | Open |
| Input methods | skipped by choice, so untested: no Japanese, Chinese or Korean keyboard has typed in it. One pasted Chinese message drew and sent, which says nothing of composing | Skipped, stops GPUI if it fails |
| Linux and Windows | needs real machines | Open |

Through a real `rein serve` with a real model, a press of Allow ran a two-line command shown whole, and a press of Reject reached the model as declined. Closing the window ended the process and `rein serve`, and stopped a background command they had started.

## What the design cost

Wearing the design takes more than the plain test window did. Side by side, two rounds of the typing gate, counting CPU seconds:

| | Keys, p50 / p99 | CPU | Peak memory | Idle memory |
|---|---|---|---|---|
| Test window, 900 by 640 | 6.3 / 11.6 ms | 22% | 158 MB | 137 to 146 MB |
| This window, 1156 by 780 | 6.7 / 13.0 ms | 31% | 195 MB | 167 MB |
| This window, at 900 by 640 | 6.7 / 12.6 ms | 28% | 159 MB | 153 MB |

The peak is all the bigger window. Idle is part window, 14 MB, and part design, 7 to 16 MB. Of the CPU, about 3 points are the pixels and 6 the design: the runs drew about as many frames, so each frame costs more. Keys are about 0.4 ms slower at p50 and 1 ms at p99. For CPU, trust this table over the one above: that one is `ps`'s average, a rougher figure.

## What the numbers don't say

Every gate ran in a window that doesn't have the keyboard, and keys went in through `dispatch_keystroke`, not the system or an input method. `key_to_paint` stops at paint, so it leaves out the GPU and the display.

`ps` is the wrong measure of memory here. It leaves out what macOS has compressed, so it reads a third to a half low, and falls with nothing freed: one reading of 57 MB idle was that. Nothing has measured growth over a long session of real turns. One conversation of 10,000 messages comes to about 230 to 290 MB, the window and `rein serve` together.

A table is held as text until it ends, then laid out whole, so one of 500 rows stalls the UI thread for 70 to 90 ms once. A row is laid out in one go: that's why code is cut every 64 lines and answers at blank lines. Tables aren't cut yet.

A real pointer over the window can start bursts of frames. In one run an idle window went from 2 frames a second to as many as 71 for about 20 seconds, at 15% CPU and 224 MB, with some of those seconds holding no pointer move at all. Run again with nothing over it, the same gate drew none, and a synthetic pointer crossing for 6 seconds adds only 8 frames. So moves alone don't draw them, and what does isn't known.

Shift-click takes all of the row clicked, not an exact character. Rows between the ends highlight only when drawn. A drag across rows that scroll away isn't handled.

Nothing here has run on Linux or Windows.

## gpui-fast

The `fast` feature uses Longbridge's faster GPUI, which calls itself experimental. It doesn't work here: without the keyboard, a window on it draws only when the pointer moves, 3 frames in a 25-second run, whatever the window's kind or frame limit, so its gate numbers mean nothing. Whether it behaves in a focused window hasn't been tried. Nothing needs it: the plain build passes.
