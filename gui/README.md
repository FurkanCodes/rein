# rein's test window

A throwaway window to find out whether GPUI can draw rein: all your agents side by side, each one to read and steer. It's a workspace of its own, so nothing of GPUI's reaches rein's dependencies or its checks. If it holds up, a macOS beta starts from what's here. If it doesn't, `replay/`, `transcript/` and `gates/` outlive it, and only `window/` is thrown away.

- `replay/` records `rein serve`, makes the workloads, and plays them back as an agent.
- `transcript/` is a conversation as a window holds it: rows, block cutting, selection and approval questions, with no GPUI in it.
- `window/` is the GPUI part, and measures itself.
- `gates/run.py` runs the gates and samples CPU and memory from outside.

## Run it

```sh
cargo run -p rein-window --release -- --agent 'rein serve --mode ask' --agent 'rein serve --mode ask'
```

Each `--agent` is a pane. The command is split on spaces, so a path with spaces in it won't work, and `rein` takes the folder you run this from as its project.

## The gates

```sh
cargo build --release -p rein-window -p rein-replay
python3 -I gates/run.py            # every gate, about 7 minutes
python3 -I gates/run.py typing     # or some of them
```

Windows float at the top right while it runs. They leave your keyboard alone. A window has to be on screen to be measured: GPUI draws nothing while it's hidden or the display is off, and a gate then reads as fast and light, so `run.py` stops if the display goes off. `REIN_GATES_FOCUSED=1` runs in a window that takes your keyboard. `REIN_WINDOW` picks another build of the window, `REIN` another rein.

The pass marks come from `reports/Fast Rust desktop GUI options.md`. They are judgement, not published numbers.

## Results

Measured on one M5 at 120 Hz, on battery, with other work running, one run of each gate. Keys are p50 / p99 / max from a key going in to the frame that shows it. Memory is the physical footprint, what Activity Monitor shows.

| Gate | Result | State |
|---|---|---|
| Typing with 3 panes replaying, one of 10,000 messages | keys 5.5 / 11.0 / 12.4 ms, longest stall 6.9 ms, 14% CPU, 154 MB | Pass |
| Idle, nothing focused | 0 frames in 60 s, 0.15 wake-ups/s, 0.0% CPU, 125 MB | Pass |
| Idle, caret in a box | 2 frames/s, 4.6 wake-ups/s, 0.1% CPU, 170 MB | Over the mark: a caret draws |
| Open and scroll 10,000 messages | opens in 50 ms, 120 fps, 0.4 ms/s of hitches against Apple's line of 10, longest frame gap 12.9 ms, 170 MB | Pass |
| Scrolling back while an answer streams | 120 fps, 0.7 ms/s of hitches, longest stall 7.6 ms, 152 MB | Pass |
| Resizing while an answer streams | keys 3.4 / 13.2 / 14.9 ms, longest stall 19.1 ms, 153 MB | Pass |
| A 200 KB answer | keys 5.0 / 10.4 / 11.8 ms, longest stall 12.1 ms, 152 MB | Pass |
| 20,000 lines of code | keys 4.4 / 8.9 / 9.0 ms, longest stall 7.6 ms, 212 MB | Pass |
| 500-row table, to its end | keys 5.9 / 10.8 / 14.1 ms, one stall of 79 to 91 ms as it ends, 172 MB | Pass at p99, see below |
| Burst of 100 KB/s | keys 6.2 / 13.7 / 22.3 ms, longest stall 13.1 ms, 145 MB | Pass |
| One byte at a time | keys 5.4 / 10.8 / 13.1 ms, longest stall 8.5 ms, 123 MB | Pass |
| Megabytes of tool output | keys 5.6 / 9.7 / 10.5 ms, longest stall 4.8 ms, 99 MB | Pass |
| Selection across rows, then scrolled away | the toolkit's goes from 816 bytes to 0, the transcript's stays at 825. Shift-click took 2 MB, 29,235 lines, in 0.7 ms | Pass, with the limits below |
| `rein serve` holding 10,000 messages | 71 MB as it opens, 39 MB after 30 s, up from 2.8, for 13.8 MB of text; opens in 283 to 673 ms | No mark set |
| macOS 26 freeze | none on macOS 26.5.2 | Pass |
| Build loop | clean 48 s debug, 78 s release; a one-line change 1 to 3 s | Pass; hot reload untried |
| Upkeep | 0.9.0 is the newest release, so there's nothing to upgrade to. This code built on 0.8.1 unchanged, before the last fixes: a week's drift | Open |
| Input methods | skipped by choice, so untested: no Japanese, Chinese or Korean keyboard has typed in it. One pasted Chinese message drew and sent, which says nothing of composing | Skipped, stops GPUI if it fails |
| Linux and Windows | needs real machines | Open |

Through a real `rein serve` with a real model, a press of Allow ran a two-line command shown whole, and a press of Reject reached the model as declined. Closing the window ended the process and `rein serve`, and stopped a background command they had started.

## What the numbers don't say

Every gate ran in a window that doesn't have the keyboard, and keys went in through `dispatch_keystroke`, not the system or an input method. `key_to_paint` stops at paint, so it leaves out the GPU and the display.

`ps` is the wrong measure of memory here. It leaves out what macOS has compressed, so it reads a third to a half low, and falls with nothing freed: one reading of 57 MB idle was that. Nothing has measured growth over a long session of real turns. One conversation of 10,000 messages comes to about 210 to 240 MB, the window and `rein serve` together.

A table is held as text until it ends, then laid out whole, so one of 500 rows stalls the UI thread for about 90 ms once. A row is laid out in one go: that's why code is cut every 64 lines and answers at blank lines. Tables aren't cut yet.

Some runs drew bursts of frames while a real pointer crossed the window, and others drew none with as many moves. What draws them isn't known: one synthetic move drew nothing.

Shift-click takes all of the row clicked, not an exact character. Rows between the ends highlight only when drawn. A drag across rows that scroll away isn't handled.

The window claims no `panes` capability, so sub-agents aren't shown. Nothing here has run on Linux or Windows.

## gpui-fast

The `fast` feature uses Longbridge's faster GPUI, which calls itself experimental. It doesn't work here: without the keyboard, a window on it draws only when the pointer moves, 3 frames in a 25-second run, whatever the window's kind or frame limit, so its gate numbers mean nothing. Whether it behaves in a focused window hasn't been tried. Nothing needs it: the plain build passes.
