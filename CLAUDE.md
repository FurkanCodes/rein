# rein: notes for anyone changing it

rein is a terminal coding agent in Rust (edition 2024, tokio `current_thread`).
`crates/rein-llm` speaks to providers (Responses, Messages, Gemini and Chat
Completions APIs), `crates/rein-agent` is the agent loop and its tools,
`crates/rein-cli` is the `rein` binary and its full-screen terminal. The README
says what rein does; this file says how its code is kept.

Every rule below is here because breaking it caused a real bug or a review
finding. Each says why in a line.

## Done means

- `cargo test`, `cargo clippy --all-targets -- -D warnings` and `cargo fmt --check`
  all pass. rustfmt is configured (`rustfmt.toml`): long lines on purpose.
- A bug fix comes with a test that fails without the fix. Break the fix on
  purpose and watch the test fail before calling it done; a test that can't fail
  proves nothing.
- Behaviour you can see gets a screen test (`crates/rein-cli/tests/screen.rs`):
  the real binary in a pseudo-terminal, against a fake model server.
- Real providers only through the opt-in suite:
  `cargo test --test e2e -- --ignored --nocapture --test-threads 1`.
- Now and then run `cargo clippy -- -W clippy::pedantic -W clippy::nursery` and
  read what's not style: unused `&mut`, values passed by value but not consumed,
  lock guards held too long, too many bools.

## Types say what's allowed

- A closed set of values is an `enum`, not a `String`. A typo then doesn't
  compile, and a `match` must handle every case. rein had tool names, effort
  levels, error codes and modes as strings; misspellings compiled, and a typo
  quietly became "max effort". See `ToolName`, `Effort`, `ErrorKind`, `Mode`.
- Parse text into the type once, at the edge (flags, settings, the model's tool
  calls, JSON from disk), with `FromStr`/serde. Deep inside, pass the type.
- Wire data stays text where the wire says text (`ToolCall::name` is the model's
  string); read it once with a constructor like `ToolName::of(call)`.
- One variant, one meaning. `Read::Line(String)` once carried typed text, a menu
  index and an answer key; a menu choice is `Read::Picked(usize)`.
- No `Option<Option<T>>`: name the cases (`EffortSetting { Default, Level }`).
- Several bools that can't all be true are one enum
  (`enum Last { Nothing, You, Step, Text }`, not three `after_*` flags).
- A rule about a set lives on the type (`ToolName::only_looks()`), not in two
  lists that must agree by hand.
- A serde enum for a closed set in tool arguments (`GrepOutput`,
  `StepStatus`): a value the model misspells comes back as an error it can fix.

## Errors are values until the edge

- Keep errors typed until `main`, and turn them into text once there
  (`describe`). Never decide anything by searching an error's text: name the
  outcome (`ErrorKind`), or a `const` both sides use (`DECLINED_COMMAND`,
  `INTERRUPTED`, `tools::ERROR`). Compaction once parsed "The user declined" out
  of tool results; rewording the message would have silently broken it.
- Keep the provider's own error code next to rein's kind; don't overwrite it.
- Implement `source()` on error types, and show the chain (`with_causes`):
  "error sending request" alone hides DNS or TLS.

## Panics and arithmetic

- No `unwrap()` outside tests. `expect("…")` only for a real invariant, saying
  which.
- Arithmetic on anything a user or a model sent is checked or saturating, and
  clamping happens before comparing: `@a.rs:0-0` once became lines 1 to 0 and
  `last - first + 1` underflowed.
- Never `expect` on the shape of a `serde_json::Value` built from data you read
  back from disk: a tampered session file must not panic.

## Ownership: borrow what you only read

- A request borrows the conversation (`Request<'a> { input: &'a [Item], .. }`).
  It used to clone the whole history on every request.
- Build once what doesn't change (the tool specs are built when the agent is).
- A "builder" method (`fn with_x(self) -> Self`) changes only its own value,
  never state shared by every clone through an `Arc`.

## Async: cancelling is dropping

- In async Rust a stopped turn is a dropped future. Cleanup belongs in `Drop`,
  not after an `.await` that may never come back.
- `tokio::spawn` detaches: dropping its `JoinHandle` does not stop the task.
  Tasks scoped to an operation go in a `JoinSet`, which stops them all when
  dropped. A cancelled sign-in once kept its callback server listening.
- A stop signal uses `Notify::notify_waiters()`. `notify_one()` stores a permit
  when nobody waits, and the next turn consumed it: Esc during a lookup stopped
  your next message.
- Nothing blocks the runtime's only thread: no `std::process::Command::status()`,
  `std::thread::sleep` or blocking reads in async code. Use `tokio::process`, a
  timeout, or `spawn_blocking`. A browser named in `$BROWSER` once could freeze
  rein until it closed.
- Every network read has a timeout: a stream idle for 5 minutes is given up on,
  a callback connection that sends nothing for 10 seconds is closed.
- Hold a std `Mutex` guard for as short as possible, and never across `.await`.

## Processes

- A command runs in its own process group, held by a guard whose `Drop` kills
  the whole group. Killing `sh` alone left `cargo test` and dev servers running
  after a timeout or Esc. A command that finishes normally disarms the guard, so
  what it put in the background on purpose keeps running.
- Read a child's output with a cap; never `wait_with_output` on something that
  can print gigabytes.

## Security

- Permission checks fail closed: what rein can't parse it doesn't allow on a
  guess. A `deny` rule was dodged by gluing the command to a `;`.
- Secrets never reach `Debug`: types holding keys or tokens (`Auth`,
  `Credentials`) have a hand-written `Debug` that leaves them out. A derived
  `Debug` prints them into any log or panic message.
- A secret typed in the terminal is never echoed, never kept in history, and the
  terminal is put back even on Ctrl-C.
- Never sign in as another app: no borrowed OAuth client IDs, no spoofed
  headers or system prompts. Official sign-ins and API keys only.

## The sandbox

- A command meant for the sandbox never runs without one: no sandbox means
  ask, not run (`Run::Sandboxed` without one is an error). Claude Code's
  default runs commands unsandboxed when its sandbox can't start.
- Rules allow a list and deny the rest (`(deny default)`), never the other way
  round: a sandbox that denied a list of bad operations was escaped through
  what the list forgot. System services are a short list for the same
  reason; allowing them all let a command reach Launch Services, which opens
  apps outside the sandbox.
- A path rule locks the folders above it too: `mv .git x`, write
  `x/hooks/pre-commit`, `mv x .git` got past a rule on `.git/hooks`.
- Protect what runs later, not only what runs now: hooks, git's config, rule
  files, editors' and agents' settings. A command that plants one escapes on
  your next commit.
- Network access is the internet, never other programs' sockets: with all
  network allowed, a command reached Docker's socket, and Docker can mount
  your home.
- Compare paths as the file system does: macOS ignores case, so `.REIN` is
  `.rein` (the sandbox resolves this itself; rein's own checks lower-case).
- Keys stay out of rein's environment (`secrets::keep`, first in `main`):
  macOS hands any process of yours another's environment (`KERN_PROCARGS2`),
  sandbox or not, and a sandboxed command read rein's key from it.
- Between fork and exec, only system calls on what's ready: no allocating,
  no locks. The Landlock ruleset is built in rein's process; the command's
  only takes it on.
- Landlock allows whole folders and can't wall off part of one, so on Linux
  what's protected inside the project is guarded (noted, then put back), and
  a rule on `/tmp` reaches a project in it: grant around what's excepted.

## The agent loop

- Stop what's stuck, not what's long. A fixed 25 steps a turn cut off real
  work once the sandbox stopped asking; a loop is the same call giving the
  same result again and again, and that's what's caught. Only a turn nobody
  watches has a step limit, and its last step asks for a report.
- A guard against rein looping on its own work goes by progress, not a
  count: compacting "once per message" overflowed long turns. Compact again
  after a step since the last time, and not after one that made no room.

## Encapsulation

- Fields that must change together are private, behind methods. `Agent`'s
  conversation (history, everything said, the session file, the window) changes
  only through `Agent::new`, `resume`, `send` and `compact`; when it had public
  fields, `/resume` set eight of them by hand, missed one, and `/undo` reverted
  the previous conversation's files.
- A value derived from others is a method, not a field that can drift.

## Files on disk

- Write files whole: a temp file with a name of its own (process and counter),
  then rename. A fixed `.tmp` name let two rein windows rename each other's
  half-written file into place.

## unsafe and FFI

- Every `unsafe` block has a `// SAFETY:` comment saying why it's sound, and is
  as small as it can be.
- A signal handler only calls async-signal-safe functions (`tcsetattr`, `write`,
  `_exit`), and whoever installs one puts back what was there before.
- Check FFI return values (`localtime_r`, `tcgetattr`).

## Structure

- A file over ~800 lines, or a function over ~80, is a sign to split. `main.rs`
  was 2,000 lines; it's now `args`, `conversation`, `terminal` and `format`.
- Code the provider formats share (grouping items by role, call IDs, parsing
  arguments) lives once, not in each format.
- `pub(crate)` unless something outside the crate needs it.
- The sandbox is its own crate, `rein-sandbox`, with no C in its
  dependencies: its tests build for Linux from a Mac and run in a container.

## Tests

- Tests never touch the real environment: a temporary `HOME`/`REIN_HOME`,
  `BROWSER=false` so nothing opens your browser, `SSH_CONNECTION` set so nothing
  touches your clipboard, servers on port 0, never a fixed port. A test once
  opened real OpenRouter sign-in pages.
- Wait for the state you need (`wait_for`, `wait_until`), never `sleep` and then
  look: a menu's title can be drawn before its list.
- Check processes by their exact command, not `pgrep -f`, which also matches the
  shell that started the test.
- An opt-in test is `#[ignore = "why"]`, so it shows as ignored, not passed.
- A sandbox test skips where there's no sandbox, so it can pass by never
  running: break the wall it checks and watch it fail. Put the file back
  newer than the broken one (`touch` it): cargo goes by modification times,
  and a copy restored with its old time kept the broken build running. Its project lives
  outside the temp folders, which every command may write; a project in one
  hid that `look` could write it.
- Check that apps are out of reach with a probe that only reads
  (`lsappinfo front`), never an Apple event: one outside the sandbox asks you
  for permission on screen.
- Visual checks in tmux use a throwaway `REIN_HOME`, so they don't add to your
  history or saved conversations.

## Docs

- Doc comments say what and why in plain words, and stay true: when the code
  changes, the comment changes with it.
